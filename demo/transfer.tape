# gritty file transfer: send from inside a session, receive on the client.
# Left pane: a gritty session. Right pane: a plain client shell.
# Run via demo/record.sh, which sets up the isolated environment and
# seeds ~/project/results.csv.

Output out/transfer.gif

Set Shell "bash"
Set FontSize 18
Set Width 1500
Set Height 700
Set Padding 16
Set WindowBar Colorful
Set TypingSpeed 80ms

# Set up a side-by-side tmux split off-camera (isolated server socket)
Hide
Type "cd"
Enter
Type "tmux -L gritty-demo new-session"
Enter
Sleep 1.5s
Ctrl+B
Type "%"
Sleep 500ms
Ctrl+B
Type "o"
Sleep 500ms
Show

Sleep 1s

# Left pane: a session holding a file worth sharing
Type "gritty connect local:work"
Sleep 500ms
Enter
Wait+Screen@10s /\[work\]/
Sleep 1s

Type "cd project && cat results.csv"
Sleep 500ms
Enter
Sleep 1.5s

# Offer the file; the sender waits for a receiver to pair with
Type "gritty send results.csv"
Sleep 500ms
Enter
Wait+Screen@10s /waiting for receiver/
Sleep 1s

# Right pane: the client picks it up
Ctrl+B
Type "o"
Sleep 500ms
Type "gritty receive"
Sleep 500ms
Enter
Wait+Screen@15s /received 1 file/
Sleep 1s

Type "cat results.csv"
Sleep 500ms
Enter
Sleep 4s
