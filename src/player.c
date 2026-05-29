#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <sys/select.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libswscale/swscale.h>
#include <libswresample/swresample.h>
#include <libavutil/opt.h>
#include <SDL2/SDL.h>

// Global playback states
static int g_paused = 0;
static int g_quit = 0;
static double g_seek_target = -1.0; // Seek target in seconds
static double g_current_time = 0.0;
static pthread_mutex_t g_lock = PTHREAD_MUTEX_INITIALIZER;

// Command reading thread
void* control_thread_fn(void* arg) {
    char line[1024];
    const char* fifo = "/tmp/aurora-player-control";
    
    // Ensure the FIFO exists
    mkfifo(fifo, 0666);
    
    while (!g_quit) {
        FILE* f = fopen(fifo, "r");
        if (!f) {
            usleep(100000);
            continue;
        }
        while (!g_quit && fgets(line, sizeof(line), f)) {
            pthread_mutex_lock(&g_lock);
            if (strncmp(line, "pause", 5) == 0 || strncmp(line, "p", 1) == 0) {
                g_paused = !g_paused;
            } else if (strncmp(line, "seek ", 5) == 0) {
                int pct = atoi(line + 5);
                g_seek_target = (double)pct / 100.0;
            } else if (strncmp(line, "quit", 4) == 0 || strncmp(line, "q", 1) == 0) {
                g_quit = 1;
            }
            pthread_mutex_unlock(&g_lock);
        }
        fclose(f);
    }
    return NULL;
}

int main(int argc, char* argv[]) {
    if (argc < 3) {
        fprintf(stderr, "Usage: %s <X11_WINDOW_ID> <MEDIA_FILE_PATH>\n", argv[0]);
        return 1;
    }

    const char* win_id_str = argv[1];
    const char* file_path = argv[2];
    unsigned long win_id = strtoul(win_id_str, NULL, 10);

    // Initialize FFmpeg Network (if playing network streams)
    avformat_network_init();

    // Open media file
    AVFormatContext* fmt_ctx = NULL;
    if (avformat_open_input(&fmt_ctx, file_path, NULL, NULL) < 0) {
        fprintf(stderr, "Error: Could not open media file %s\n", file_path);
        return 1;
    }

    if (avformat_find_stream_info(fmt_ctx, NULL) < 0) {
        fprintf(stderr, "Error: Could not find stream info\n");
        return 1;
    }

    // Find video and audio streams
    int video_stream_idx = -1;
    int audio_stream_idx = -1;
    for (unsigned int i = 0; i < fmt_ctx->nb_streams; i++) {
        if (fmt_ctx->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_VIDEO && video_stream_idx < 0) {
            video_stream_idx = i;
        }
        if (fmt_ctx->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_AUDIO && audio_stream_idx < 0) {
            audio_stream_idx = i;
        }
    }

    AVCodecContext* video_dec_ctx = NULL;
    if (video_stream_idx >= 0) {
        AVCodecParameters* codec_par = fmt_ctx->streams[video_stream_idx]->codecpar;
        const AVCodec* codec = avcodec_find_decoder(codec_par->codec_id);
        if (codec) {
            video_dec_ctx = avcodec_alloc_context3(codec);
            avcodec_parameters_to_context(video_dec_ctx, codec_par);
            if (avcodec_open2(video_dec_ctx, codec, NULL) < 0) {
                avcodec_free_context(&video_dec_ctx);
                video_dec_ctx = NULL;
            }
        }
    }

    AVCodecContext* audio_dec_ctx = NULL;
    if (audio_stream_idx >= 0) {
        AVCodecParameters* codec_par = fmt_ctx->streams[audio_stream_idx]->codecpar;
        const AVCodec* codec = avcodec_find_decoder(codec_par->codec_id);
        if (codec) {
            audio_dec_ctx = avcodec_alloc_context3(codec);
            avcodec_parameters_to_context(audio_dec_ctx, codec_par);
            if (avcodec_open2(audio_dec_ctx, codec, NULL) < 0) {
                avcodec_free_context(&audio_dec_ctx);
                audio_dec_ctx = NULL;
            }
        }
    }

    if (!video_dec_ctx && !audio_dec_ctx) {
        fprintf(stderr, "Error: Could not open decoders for either video or audio streams\n");
        return 1;
    }

    // Set SDL window environment variable to embed inside X11 subwindow
    char sdl_win_env[128];
    snprintf(sdl_win_env, sizeof(sdl_win_env), "SDL_WINDOWID=%lu", win_id);
    putenv(sdl_win_env);

    // Initialize SDL2 Video & Audio
    if (SDL_Init(SDL_INIT_VIDEO | SDL_INIT_AUDIO) < 0) {
        fprintf(stderr, "Error: SDL_Init failed: %s\n", SDL_GetError());
        return 1;
    }

    // Create SDL window wrapping the X11 Window ID
    SDL_Window* window = SDL_CreateWindowFrom((void*)win_id);
    if (!window) {
        fprintf(stderr, "Error: SDL_CreateWindowFrom failed: %s\n", SDL_GetError());
        return 1;
    }

    int win_w = 516, win_h = 278; // Default child window dimensions
    SDL_GetWindowSize(window, &win_w, &win_h);

    SDL_Renderer* renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_ACCELERATED | SDL_RENDERER_PRESENTVSYNC);
    if (!renderer) {
        renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_SOFTWARE);
    }

    SDL_Texture* video_texture = NULL;
    if (video_dec_ctx) {
        video_texture = SDL_CreateTexture(
            renderer,
            SDL_PIXELFORMAT_YV12,
            SDL_TEXTUREACCESS_STREAMING,
            video_dec_ctx->width,
            video_dec_ctx->height
        );
    }

    // Set up Audio Resampler & Device
    SDL_AudioDeviceID audio_device = 0;
    struct SwrContext* swr_ctx = NULL;
    if (audio_dec_ctx) {
        SDL_AudioSpec wanted_spec, obtained_spec;
        SDL_memset(&wanted_spec, 0, sizeof(wanted_spec));
        wanted_spec.freq = audio_dec_ctx->sample_rate;
        wanted_spec.format = AUDIO_S16SYS; // Signed 16-bit system byte-order PCM
        wanted_spec.channels = audio_dec_ctx->ch_layout.nb_channels;
        wanted_spec.samples = 1024;

        audio_device = SDL_OpenAudioDevice(NULL, 0, &wanted_spec, &obtained_spec, 0);
        if (audio_device > 0) {
            SDL_PauseAudioDevice(audio_device, 0); // Unpause
            
            // Set up audio resampling to standard S16 format
            swr_ctx = swr_alloc();
            av_opt_set_chlayout(swr_ctx, "in_chlayout", &audio_dec_ctx->ch_layout, 0);
            av_opt_set_int(swr_ctx, "in_sample_rate", audio_dec_ctx->sample_rate, 0);
            av_opt_set_sample_fmt(swr_ctx, "in_sample_fmt", audio_dec_ctx->sample_fmt, 0);
            
            AVChannelLayout out_layout;
            av_channel_layout_default(&out_layout, obtained_spec.channels);
            av_opt_set_chlayout(swr_ctx, "out_chlayout", &out_layout, 0);
            av_opt_set_int(swr_ctx, "out_sample_rate", obtained_spec.freq, 0);
            av_opt_set_sample_fmt(swr_ctx, "out_sample_fmt", AV_SAMPLE_FMT_S16, 0);
            
            swr_init(swr_ctx);
            av_channel_layout_uninit(&out_layout);
        }
    }

    // Spawn control thread
    pthread_t control_thread;
    pthread_create(&control_thread, NULL, control_thread_fn, NULL);

    AVPacket* packet = av_packet_alloc();
    AVFrame* frame = av_frame_alloc();
    AVFrame* audio_frame = av_frame_alloc();

    // Timers & Synchronizers
    double time_base_video = video_dec_ctx ? av_q2d(fmt_ctx->streams[video_stream_idx]->time_base) : 1.0;
    double time_base_audio = audio_dec_ctx ? av_q2d(fmt_ctx->streams[audio_stream_idx]->time_base) : 1.0;
    double duration = (double)fmt_ctx->duration / AV_TIME_BASE;
    if (duration <= 0.0) duration = 1.0;
    uint32_t start_ticks = SDL_GetTicks();
    double seek_target = -1.0;
    uint32_t last_progress_write = 0;

    // Main playback loop
    while (!g_quit) {
        pthread_mutex_lock(&g_lock);
        int is_paused = g_paused;
        if (g_seek_target >= 0.0) {
            seek_target = g_seek_target * duration;
            g_seek_target = -1.0;
        }
        pthread_mutex_unlock(&g_lock);

        // Process Seek
        if (seek_target >= 0.0) {
            int64_t target_ts = (int64_t)(seek_target * AV_TIME_BASE);
            if (av_seek_frame(fmt_ctx, -1, target_ts, AVSEEK_FLAG_BACKWARD) >= 0) {
                if (video_dec_ctx) avcodec_flush_buffers(video_dec_ctx);
                if (audio_dec_ctx) avcodec_flush_buffers(audio_dec_ctx);
                if (audio_device > 0) SDL_ClearQueuedAudio(audio_device);
                start_ticks = SDL_GetTicks() - (uint32_t)(seek_target * 1000.0);
            }
            seek_target = -1.0;
        }

        if (is_paused) {
            if (audio_device > 0) SDL_PauseAudioDevice(audio_device, 1);
            SDL_Delay(10);
            start_ticks += 10; // Slide base ticks so video doesn't jump forward
            continue;
        } else {
            if (audio_device > 0) SDL_PauseAudioDevice(audio_device, 0);
        }

        // Read packet
        if (av_read_frame(fmt_ctx, packet) < 0) {
            // End of file, loop back automatically
            av_seek_frame(fmt_ctx, -1, 0, AVSEEK_FLAG_BACKWARD);
            if (video_dec_ctx) avcodec_flush_buffers(video_dec_ctx);
            if (audio_dec_ctx) avcodec_flush_buffers(audio_dec_ctx);
            if (audio_device > 0) SDL_ClearQueuedAudio(audio_device);
            start_ticks = SDL_GetTicks();
            continue;
        }

        // Decode Video
        if (packet->stream_index == video_stream_idx && video_dec_ctx) {
            if (avcodec_send_packet(video_dec_ctx, packet) == 0) {
                while (avcodec_receive_frame(video_dec_ctx, frame) == 0) {
                    double pts = frame->best_effort_timestamp * time_base_video;

                    // Standard Wall-Clock Synchronization
                    uint32_t elapsed = SDL_GetTicks() - start_ticks;
                    double expected = pts * 1000.0;
                    if (expected > elapsed) {
                        SDL_Delay((uint32_t)(expected - elapsed));
                    }

                    pthread_mutex_lock(&g_lock);
                    g_current_time = pts;
                    pthread_mutex_unlock(&g_lock);

                    // Update YUV texture directly via GPU
                    SDL_UpdateYUVTexture(
                        video_texture,
                        NULL,
                        frame->data[0], frame->linesize[0],
                        frame->data[1], frame->linesize[1],
                        frame->data[2], frame->linesize[2]
                    );

                    // Draw video centered in child viewport
                    SDL_RenderClear(renderer);
                    SDL_RenderCopy(renderer, video_texture, NULL, NULL);
                    SDL_RenderPresent(renderer);
                }
            }
        }
        // Decode Audio
        else if (packet->stream_index == audio_stream_idx && audio_dec_ctx && swr_ctx) {
            if (avcodec_send_packet(audio_dec_ctx, packet) == 0) {
                while (avcodec_receive_frame(audio_dec_ctx, audio_frame) == 0) {
                    // Track audio time for audio-only files
                    if (!video_dec_ctx && audio_frame->best_effort_timestamp != AV_NOPTS_VALUE) {
                        double audio_pts = audio_frame->best_effort_timestamp * time_base_audio;
                        pthread_mutex_lock(&g_lock);
                        g_current_time = audio_pts;
                        pthread_mutex_unlock(&g_lock);
                    }

                    int max_out_samples = swr_get_out_samples(swr_ctx, audio_frame->nb_samples);
                    uint8_t* out_buf = NULL;
                    av_samples_alloc(&out_buf, NULL, audio_dec_ctx->ch_layout.nb_channels, max_out_samples, AV_SAMPLE_FMT_S16, 0);
                    
                    int out_samples = swr_convert(
                        swr_ctx,
                        &out_buf,
                        max_out_samples,
                        (const uint8_t**)audio_frame->data,
                        audio_frame->nb_samples
                    );

                    if (out_samples > 0) {
                        int size = out_samples * 2 * audio_dec_ctx->ch_layout.nb_channels;
                        // Avoid audio buffer overflow
                        while (SDL_GetQueuedAudioSize(audio_device) > 65536 && !g_quit) {
                            SDL_Delay(5);
                            start_ticks += 5;
                        }
                        SDL_QueueAudio(audio_device, out_buf, size);
                    }
                    av_freep(&out_buf);
                }
            }
        }

        av_packet_unref(packet);

        // Write progress every ~100ms to progress file
        uint32_t now_ticks = SDL_GetTicks();
        if (now_ticks - last_progress_write >= 100) {
            last_progress_write = now_ticks;
            pthread_mutex_lock(&g_lock);
            double ct = g_current_time;
            pthread_mutex_unlock(&g_lock);
            double progress = (duration > 0.0) ? (ct / duration) : 0.0;
            if (progress < 0.0) progress = 0.0;
            if (progress > 1.0) progress = 1.0;
            FILE* pf = fopen("/tmp/aurora-player-progress", "w");
            if (pf) {
                fprintf(pf, "%.6f\n", progress);
                fclose(pf);
            }
        }

        // Basic event pump to prevent window from freezing
        SDL_Event event;
        while (SDL_PollEvent(&event)) {
            if (event.type == SDL_QUIT) {
                g_quit = 1;
            }
        }
    }

    // Cleanup resources
    av_packet_free(&packet);
    av_frame_free(&frame);
    av_frame_free(&audio_frame);

    if (video_dec_ctx) avcodec_free_context(&video_dec_ctx);
    if (audio_dec_ctx) avcodec_free_context(&audio_dec_ctx);
    if (swr_ctx) swr_free(&swr_ctx);
    avformat_close_input(&fmt_ctx);

    if (audio_device > 0) SDL_CloseAudioDevice(audio_device);
    if (video_texture) SDL_DestroyTexture(video_texture);
    SDL_DestroyRenderer(renderer);
    SDL_DestroyWindow(window);
    SDL_Quit();

    return 0;
}
