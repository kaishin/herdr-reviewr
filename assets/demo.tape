# The demo recording, through the installed release; needs git, herdr, jq, vhs. Regenerate with:
# VERSION=$(awk -F'"' '/^version = / {print $2; exit}' herdr-plugin.toml) && herdr plugin install kaishin/herdr-reviewr --ref "v$VERSION" --yes && vhs assets/demo.tape

Require git
Require herdr
Require jq

Output assets/demo.gif

Set Shell "bash"
Set FontSize 13
Set Width 1320
Set Height 820
Set Theme "Catppuccin Mocha"
Set Padding 16
Set TypingSpeed 45ms

# --- Hidden setup: a temp repo with a committed baseline + an uncommitted edit to review ---
Hide
Type "BIN=$(bash assets/demo-bin.sh) && bash assets/demo-fixture.sh /tmp/herdr-reviewr-demo || exit 1" Enter
Type "bash /tmp/herdr-reviewr-demo/.git/reviewr-demo/demo-session $BIN || exit 1" Enter
Show

# --- The review loop ---
Sleep 2s

# Focus the diff and move into the changed region.
Tab
Sleep 700ms
Type "jjjj"
Sleep 900ms

# Select a line range...
Type "v"
Sleep 400ms
Type "jj"
Sleep 700ms

# ...and comment on it.
Type "c"
Sleep 600ms
Type "guard the empty value here too?"
Sleep 800ms
Enter
Sleep 1200ms

# Send the review. reviewr exports it to the agent input and confirms the handoff in the footer.
Type "s"
Sleep 2800ms
