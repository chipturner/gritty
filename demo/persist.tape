# gritty hero demo: a session keeps running while you're gone.
# Run via demo/record.sh, which sets up the isolated environment
# (socket dir, config, HOME) and seeds background sessions.

Output out/persist.gif

Set Shell "bash"
Set FontSize 18
Set Width 1500
Set Height 700
Set Padding 16
Set WindowBar Colorful
Set TypingSpeed 80ms

Sleep 500ms

# Create a session and start some work
Type "gritty connect local:demo"
Sleep 500ms
Enter
Wait+Screen@10s /\[demo\]/
Sleep 1s

Type 'i=0; while sleep 1; do echo "tick $((i+=1))"; done'
Sleep 500ms
Enter
Wait+Screen@15s /tick 3/
Sleep 500ms

# Detach with the ~. escape (newline arms it)
Enter
Sleep 300ms
Type "~."
Sleep 1.5s
Type "clear"
Enter
Sleep 500ms

# The session is still there -- and still ticking
Type "gritty ls"
Sleep 500ms
Enter
Sleep 3s

# Reattach: the work never stopped
Type "gritty connect local:demo"
Sleep 500ms
Enter
Wait+Screen@20s /tick 9/
Sleep 2s

# Wind down
Ctrl+C
Sleep 1s
Type "exit"
Sleep 300ms
Enter
Sleep 2s
