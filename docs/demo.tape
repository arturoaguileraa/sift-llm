# Sift demo recording — generate with:  vhs docs/demo.tape
# Requires: https://github.com/charmbracelet/vhs  (brew install vhs)
# Adjust the commands to match the real CLI once Phase 1 lands.

Output docs/demo.gif

Set FontSize 18
Set Width 1200
Set Height 720
Set Padding 20
Set Theme "Dracula"
Set TypingSpeed 60ms

# --- Intro -------------------------------------------------------------
Type "# Sift: strip PII before it leaves your machine"
Enter
Sleep 800ms

# --- Start the gateway -------------------------------------------------
Type "export ANTHROPIC_API_KEY=sk-ant-***   # real key stays local"
Enter
Sleep 400ms
Type "sift --config policies.yaml"
Enter
Sleep 1200ms
# (expected: 'Sift listening on http://localhost:8787  [mode: shadow]')

# --- Show a prompt that contains a secret ------------------------------
Type "# A prompt with a real secret and an email"
Enter
Sleep 500ms
Type 'echo "deploy with token ghp_S3cr3tT0k3n and notify juan@empresa.com" | sift scan'
Enter
Sleep 1500ms
# (expected shadow output:)
#   [detect] api_key     -> would BLOCK   ghp_****
#   [detect] email       -> would REDACT  [EMAIL_REDACTED]

# --- Same request through the proxy ------------------------------------
Type "# Now the same flowing through the gateway from opencode"
Enter
Sleep 600ms
Type "opencode run 'summarize deploy.sh'"
Enter
Sleep 2000ms
# (expected: response comes back; secrets never reached the provider)

Sleep 1500ms
