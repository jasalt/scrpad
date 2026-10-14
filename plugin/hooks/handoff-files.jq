# True when a Claude Code tool call names a file scrpad hands to a project: `.env`, or a name
# like `.env.local` (but not `.env.example`). Add your own names to the alternatives in `files`,
# as in `|config\\.json`.
#
# A scrpad command that names the note, `scrpad --append .env -`, `--write .env DIR` or
# `--run .env -- CMD`, is the round trip itself: scrpad asks the user first. Only that note
# title is passed over; the rest of the command is checked like any other text. A title the
# shell would run code in, with `$`, a backtick, `(` or `\` outside single quotes, is no title:
# `scrpad --append "$(cat .env)" -` is checked whole.
def files: "\\.env(\\.(?!example)[A-Za-z0-9_.-]+)?";
def names_a_file: test("(^|[/\\s\"'=~<`])(" + files + ")($|[\\s\"';|&)<>`])");
def without_scrpad_titles: gsub("scrpad(\\.com|\\.exe)?\\s+--(append|write|run)\\s+(\"[^\"$`(\\\\]*\"|'[^']*'|[^\\s;|&$`(\\\\\"']+)(?=$|[\\s;|&])"; "scrpad");

[.tool_input | .. | strings | without_scrpad_titles] | any(names_a_file)
