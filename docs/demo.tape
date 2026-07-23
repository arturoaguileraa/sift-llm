# Sift demo recording — driven by the REAL `sift` binary (no mocks).
# Requires: brew install vhs zsh-syntax-highlighting
# Build first, then run from the repo root:
#   cargo build && vhs docs/demo.tape

Output "docs/demo.gif"

Set Shell "zsh"
Set FontSize 18
Set Width 1160
Set Height 900
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Hide
Type "source /opt/homebrew/share/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh" Enter
Type "setopt interactive_comments" Enter
Type "ZSH_HIGHLIGHT_STYLES[command]='fg=#cba6f7,bold'" Enter
Type "ZSH_HIGHLIGHT_STYLES[precommand]='fg=#cba6f7,bold'" Enter
Type "export PATH=$PWD/target/debug:/opt/homebrew/bin:$PATH" Enter
Type "clear" Enter
Show

Type "# 1. Launch the gateway (your API keys never leave this machine)" Enter
Sleep 500ms
Type "sift serve --config policies.yaml &" Enter
Sleep 1700ms

Enter
Sleep 400ms

Type "# 2. Every model it exposes is PII-secured" Enter
Sleep 500ms
Type "sift models" Enter
Sleep 1900ms

Enter
Sleep 400ms

Type "# 3. It answers as a real OpenAI-compatible endpoint" Enter
Sleep 500ms
Type "curl -s localhost:8787/v1/models | python3 -m json.tool | head -9" Enter
Sleep 2000ms

Enter
Sleep 400ms

Type "# 4. Nothing sensitive leaves: redaction in action" Enter
Sleep 500ms
Type "sift scan --config docs/demo.policies.yaml docs/leak.sample.txt" Enter
Sleep 2800ms

Hide
Type "kill %1 2>/dev/null" Enter
