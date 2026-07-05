# gritty session management: list, background creation, kill by name.
# Run via demo/record.sh, which sets up the isolated environment
# (socket dir, config, HOME) and seeds background sessions.

Output out/sessions.gif

Set Shell "bash"
Set FontSize 18
Set Width 1500
Set Height 700
Set Padding 16
Set WindowBar Colorful
Set TypingSpeed 80ms

Sleep 500ms

# Seeded sessions are already running
Type "gritty ls"
Sleep 500ms
Enter
Wait+Screen@10s /scratch/
Sleep 2.5s

# Start a background job in its own session, without attaching
Type "gritty connect -d local:deploy -c 'sleep 300'"
Sleep 500ms
Enter
Sleep 1.5s

Type "gritty ls"
Sleep 500ms
Enter
Wait+Screen@10s /deploy/
Sleep 3s

# Kill sessions by bare name
Type "gritty kill deploy build"
Sleep 500ms
Enter
Sleep 1.5s

Type "gritty ls"
Sleep 500ms
Enter
Sleep 3s
