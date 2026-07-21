# PREVIEW demo generator (mock `sift` + mock `opencode` picker).
# Output is plain; the typed command is coloured by zsh-syntax-highlighting.
# Requires:  brew install vhs gum zsh-syntax-highlighting
# Run from this folder:  cd docs/preview && vhs preview.tape

Output "../demo.gif"

Set Shell "zsh"
Set FontSize 18
Set Width 1120
Set Height 880
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Hide
Type "source /opt/homebrew/share/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh" Enter
Type "setopt interactive_comments" Enter
Type "ZSH_HIGHLIGHT_STYLES[command]='fg=#cba6f7,bold'" Enter
Type "ZSH_HIGHLIGHT_STYLES[precommand]='fg=#cba6f7,bold'" Enter
Type "export PATH=/opt/homebrew/bin:$PWD:$PATH" Enter
Type "clear" Enter
Show

Type "# 1. Launch the gateway" Enter
Sleep 500ms
Type "sift serve --config policies.yaml" Enter
Sleep 1500ms

Enter
Sleep 400ms

Type "# 2. Add providers - the gateway discovers their models" Enter
Sleep 500ms
Type "sift provider add" Enter
Sleep 1300ms
Enter
Sleep 900ms
Type "sift provider add" Enter
Sleep 1300ms
Down Down Down Down Down Down
Sleep 700ms
Enter
Sleep 700ms
Type "https://api.groq.com/openai/v1" Enter
Sleep 1000ms

Enter
Sleep 400ms

Type "# 3. Add the gateway to OpenCode as one provider (your endpoint)" Enter
Sleep 500ms
Type "cat opencode.json" Enter
Sleep 1800ms

Enter
Sleep 400ms

Type "# 4. OpenCode lists every discovered model:" Enter
Sleep 500ms
Type "opencode" Enter
Sleep 2000ms
Down
Sleep 1000ms
Enter
Sleep 1600ms
