# Sift demo — console segment (provider registration) for docs/demo.gif.
# The final docs/demo.gif is this clip followed by a screen recording of
# OpenCode using Sift, concatenated with ffmpeg. Render this part with:
#   cargo build --release && vhs docs/demo-console.tape
# A throwaway HOME is used so the placeholder key never touches your real
# ~/.config/sift/providers.json.

Output "docs/demo-console.gif"

Set Shell "zsh"
Set FontSize 18
Set Width 1200
Set Height 300
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms

Hide
Type "export HOME=/tmp/sift" Enter
Type "source /opt/homebrew/share/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh" Enter
Type "setopt interactive_comments" Enter
Type "ZSH_HIGHLIGHT_STYLES[command]='fg=#cba6f7,bold'" Enter
Type "ZSH_HIGHLIGHT_STYLES[precommand]='fg=#cba6f7,bold'" Enter
Type "export PATH=$PWD/target/release:/opt/homebrew/bin:$PATH" Enter
Type "clear" Enter
Show

Type "# Register your LLM provider with Sift. Your API key stays on this machine." Enter
Sleep 700ms
Type "sift provider add --name google --api-key AIzaSyD3M0pl4c3h0ld3rKEYxxxxxxxxxxxx" Enter
Sleep 3200ms
