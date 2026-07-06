# gritty port forwarding: bring a session-side web server to the client.
# Left pane: a gritty session serving HTTP. Right pane: a plain client
# shell placing the forward and curling through it.
# Run via demo/record.sh, which sets up the isolated environment and
# seeds ~/site/index.html.

Output out/forward.gif

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

# Left pane: a web server inside a session
Type "gritty connect local:web"
Sleep 500ms
Enter
Wait+Screen@10s /\[web\]/
Sleep 1s

Type "cd site && python3 -m http.server 8000"
Sleep 500ms
Enter
Wait+Screen@10s /Serving HTTP/
Sleep 1s

# Right pane: forward the session's port 8000 to local port 9000
Ctrl+B
Type "o"
Sleep 500ms
Type "gritty rf 9000:8000 &"
Sleep 500ms
Enter
Wait+Screen@10s /active/
Sleep 500ms
# Fresh prompt line -- the forward's status line printed over the last one
Enter
Sleep 500ms

# Fetch through the forward; the server pane logs the hit
Type "curl -s localhost:9000"
Sleep 500ms
Enter
Wait+Screen@10s /hello from the session/
Sleep 4s
