after finish each task, rebuild and run or stop and restart  this wm at :11, it have xserver started there, when you need test gui, use xdotool at :11 too

#linux computer use
first get current screenshot then you can controll linux via xdotool, only use folowing xdotool to avoid misuse
## first get screen info
xdotool getdisplaygeometry

## do not put any other cmd after type, just put text it only accept text
xdotool type "text" 
# type keys same time
xdotool key ctrl+shift+a
#alway put keyup after keydown
xdotool keydown shift a keyup shift
## always put mouseup afte mousedown
xdotool mousemove 200 300
xdotool mousemove 200 220 click 1  # mv and click 
 # use this for mouse drag we need sleep 1 to drag correctly
xdotool mousedown 1 sleep 1 mousemove 300 100  mouseup 1
xdotool mousedown 3 mouseup 3 # right mouse
##  reset script if your keys get stuck,alway reset after finish a step:
xdotool keyup super keyup ctrl keyup alt keyup shift mouseup 1 mouseup 2

