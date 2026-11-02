use super::{auto_run_programs, classify, LocalRisk};

fn refusal(cmd: &str) -> Option<String> {
    auto_run_programs(cmd).err()
}

fn assert_runs(cmds: &[&str]) {
    for cmd in cmds {
        assert_eq!(refusal(cmd), None, "{cmd:?} was refused");
    }
}

fn assert_asks(cmds: &[&str]) {
    for cmd in cmds {
        assert!(refusal(cmd).is_some(), "{cmd:?} may run unasked");
    }
}

// Every allowed form of the list (research card, «Команды и флаги»).
#[test]
fn allowed_forms_run() {
    assert_runs(&[
        "ls",
        "ls -la",
        "ls -1",
        "ls -R src",
        "ls -lhtr src",
        "ls --color",
        "ls --color=auto src",
        "cat a.txt",
        "cat -n a.txt b.txt",
        "head -n 20 a.txt",
        "head -n20 a.txt",
        "head -c 100 a.txt",
        "head -q -v a.txt",
        "tail -n 5 a.txt",
        "tail -r a.txt",
        "wc -l a.txt",
        "wc -lwc a.txt",
        "stat a.txt",
        "stat -L a.txt",
        "stat -c %s a.txt",
        "stat --format=%s a.txt",
        "stat --printf %s a.txt",
        "stat --terse a.txt",
        "file a.txt",
        "file -b a.txt",
        "file --mime-type a.txt",
        "file --mime-encoding a.txt",
        "grep foo a.txt",
        "grep -rn foo .",
        "grep -r -i foo src",
        "grep -rnI foo src",
        "grep -e foo -r .",
        "grep -A3 foo a.txt",
        "grep -A 3 -B 2 -C 1 foo a.txt",
        "grep -m 5 foo a.txt",
        "grep -f patterns.txt a.txt",
        "grep -r foo . --include=*.rs",
        "grep -r foo . --exclude-dir target",
        "grep -r foo . --exclude=*.lock",
        "grep --color=never foo a.txt",
        "rg foo",
        "rg -i foo src",
        "rg -l foo",
        "rg -n -w foo",
        "rg -g '*.rs' foo",
        "rg -t rust foo",
        "rg -T js foo",
        "rg -C2 foo",
        "rg -m 3 foo",
        "rg -d 2 foo",
        "rg --max-depth 2 foo",
        "rg --max-depth=2 foo",
        "rg --files src",
        "rg --count foo",
        "rg --json foo",
        "rg --vimgrep foo",
        "rg --heading foo",
        "rg --no-heading foo",
        "rg --sort=path foo",
        "rg --no-config foo",
        "find . -name '*.rs'",
        "find src -type f -maxdepth 2",
        "find . -name a -o -name b",
        "find . -not -path './target/*' -print",
        "find -H . -empty",
        "find -P . -print0",
        "find . -newer a.txt -ls",
        "find . -size +1M -mtime -2 -mmin -30",
        "find . -iname '*.md' -and -ipath '*docs*'",
        "find . -regex '.*rs' -or -iregex '.*md'",
        "find . -mindepth 1 -prune",
        "find . -printf '%p\\n'",
        "find . '(' -name a -o -name b ')' -print",
        "find . ! -name a",
        "tree",
        "tree -L 2",
        "tree -a -I target",
        "tree -P '*.rs' src",
        "tree -adfisDhFCnpug",
        "tree -J",
        "tree -X",
        "tree --gitignore --noreport --dirsfirst src",
        "tree --charset=ascii",
        "tree --charset ascii",
        "sort a.txt",
        "sort -n -r a.txt",
        "sort -nru a.txt",
        "sort -t, -k2 a.txt",
        "sort -k 2 a.txt",
        "sort -c a.txt",
        "echo hello",
        "echo -n hi",
        "pwd",
        "pwd -P",
        "pwd -L",
        "which rg",
        "which cat ls",
    ]);
}

#[test]
fn stages_each_pass_the_list() {
    assert_runs(&[
        "cat a.txt | grep foo | sort | head -n 5",
        "ls; pwd",
        "ls && pwd",
        "ls || pwd",
    ]);
    assert_asks(&["ls | git status", "cat a.txt && ps aux", "pwd; tree -l"]);
}

#[test]
fn reports_the_programs_it_runs() {
    assert_eq!(
        auto_run_programs("cat a.txt | grep foo | sort"),
        Ok(vec!["cat", "grep", "sort"])
    );
}

// Acceptance: no form of the research card's «Проба» table runs unasked.
#[test]
fn probe_table_forms_ask() {
    assert_asks(&[
        "node version",
        "npx version",
        "java version",
        "rustc version",
        "dotnet version",
        "yarn version",
        "pnpm version",
        "yarn --version",
        "cargo --version",
        "go version",
        "git status",
        "git diff",
        "git log -p -3",
        "git diff --ext-diff",
        "git log --textconv -p",
        "ps e",
        "ps -E",
        "ps aux",
        "grep -R",
        "grep -S -r",
        "find -L",
        "find . -follow",
        "rg -L",
        "tree -l",
        "ls -RL",
        "tree -R -H . -L 2",
        "file -C -m magic",
        "file -f list.txt",
        "sort --files0-from=list",
        "wc --files0-from=list",
        "find . -files0-from list",
        "rg -z",
        "more -k keys.txt a.txt",
        "tail -f log.txt",
        "cat .npmrc",
        "cat .git/config",
        "cat id_rsa",
        "head .netrc",
        "cat .env",
        "gm",
        "write",
        "gps",
        "get-process",
        "hostname newname",
    ]);
}

// Commands outside the list, and the reason names the command.
#[test]
fn commands_off_the_list_ask() {
    assert_asks(&[
        "git",
        "git status",
        "git log --oneline",
        "node -v",
        "npm --version",
        "python3 --version",
        "ps",
        "more a.txt",
        "whoami",
        "hostname",
        "type ls",
        "findstr foo a.txt",
        "ver",
        "Get-ChildItem src",
        "gci",
        "dir",
        "select-string foo a.txt",
        "winget list",
        "rm a.txt",
        "uniq a.txt",
    ]);
    let why = refusal("git status").unwrap();
    assert!(why.contains("`git`"), "{why}");
}

// The first word as written: no path, no wrapper, no other case or
// extension, no leading `.` or `&` (review: `norm_cmd` and `command_tokens`
// would hide them).
#[test]
fn command_name_is_the_raw_first_word() {
    assert_asks(&[
        "/bin/ls",
        "./ls",
        "bin/cat a.txt",
        "Cat a.txt",
        "LS",
        "cat.com a.txt",
        "ls.exe",
        "'ls'",
        "\\ls",
        ". ls",
        "& ls",
        "env ls",
        "X=1 ls",
        "xargs cat",
        "command ls",
        "nohup cat a.txt",
        "bash -c ls",
        "sh -c 'cat a.txt'",
    ]);
}

// Flags are compared with their case (research card: `-L`/`-l`,
// `-R`/`-r`).
#[test]
fn flags_are_case_sensitive() {
    assert_runs(&["tree -L 2", "grep -r foo .", "rg -l foo"]);
    assert_asks(&["tree -l", "grep -R foo .", "rg -L foo"]);
}

// The deny column of the list, one plain form per flag.
#[test]
fn denied_flags_ask() {
    assert_asks(&[
        "ls -L",
        "ls --dereference",
        "tail -f a.txt",
        "tail -F a.txt",
        "tail --follow a.txt",
        "tail --retry a.txt",
        "tail --pid 1 a.txt",
        "wc --files0-from=list",
        "file -f list.txt",
        "file --files-from list.txt",
        "file -C",
        "file --compile",
        "file -m magic a.txt",
        "file --magic-file magic a.txt",
        "file -z a.gz",
        "file -Z a.gz",
        "grep --dereference-recursive foo .",
        "grep -S foo .",
        "grep -z foo a.txt",
        "grep -Z foo a.txt",
        "grep -J foo a.txt",
        "grep -d skip foo a.txt",
        "grep -D skip foo a.txt",
        "rg --follow foo",
        "rg --pre cat foo",
        "rg --pre-glob '*.gz' foo",
        "rg --hostname-bin x foo",
        "rg --search-zip foo",
        "find . -exec cat",
        "find . -execdir cat",
        "find . -ok cat",
        "find . -okdir cat",
        "find . -delete",
        "find . -fprint out.txt",
        "find . -fprint0 out.txt",
        "find . -fprintf out.txt %p",
        "find . -fls out.txt",
        "tree -o out.txt",
        "tree --output out.txt",
        "tree -R",
        "tree -H .",
        "tree --fromfile list.txt",
        "sort -o out.txt a.txt",
        "sort --output=out.txt a.txt",
        "sort --compress-program=gzip a.txt",
        "sort --files0-from list a.txt",
        "sort -T tmp a.txt",
        "sort --temporary-directory=tmp a.txt",
        "sort --random-source=a.txt a.txt",
        "which -a ls",
    ]);
    let why = refusal("tree -l").unwrap();
    assert!(why.contains("`-l`") && why.contains("`tree`"), "{why}");
}

// User decision 4 of 28.10: rg into hidden or ignored files asks; recursive
// grep does not.
#[test]
fn rg_hidden_and_ignored_ask_grep_r_runs() {
    assert_asks(&[
        "rg --hidden foo",
        "rg -. foo",
        "rg -u foo",
        "rg -uu foo",
        "rg -uuu foo",
        "rg --no-ignore foo",
        "rg --no-ignore-vcs foo",
    ]);
    assert_runs(&["grep -r foo .", "grep -rn foo src"]);
}

// Short flags in a cluster are read one by one, with their values.
#[test]
fn clusters_are_read_flag_by_flag() {
    assert_runs(&[
        "grep -rn foo .",
        "grep -rnA3 foo .",
        "ls -la",
        "head -n20 a.txt",
    ]);
    assert_asks(&["grep -rnS foo .", "ls -laL", "rg -iL foo"]);
    let why = refusal("grep -rnS foo .").unwrap();
    assert!(why.contains("`-S`"), "{why}");
}

// tree's own parser takes a value from the next word and goes on with the
// cluster, so a value never comes from the cluster itself.
#[test]
fn tree_value_flags_take_the_next_word_only() {
    assert_runs(&["tree -aL 2", "tree -L 2 -a"]);
    assert_asks(&["tree -L2", "tree -La 2"]);
}

// A value word is the flag's value, not a flag.
#[test]
fn flag_values_are_not_flags() {
    assert_runs(&["grep -e -R a.txt", "head -n -5 a.txt", "find . -name -L"]);
}

// Long flags only by their full name (GNU takes abbreviations).
#[test]
fn abbreviated_long_flags_ask() {
    assert_asks(&[
        "grep --incl=*.rs -r foo .",
        "sort --fil=list a.txt",
        "rg --max-dep 2 foo",
        "tree --dirs src",
        "ls --col",
    ]);
}

#[test]
fn unknown_flags_ask() {
    assert_asks(&[
        "ls -Q",
        "cat --number a.txt",
        "head -20 a.txt",
        "wc --lines a.txt",
        "pwd -x",
        "rg --colors foo",
        "find . -quit",
        "grep --heading=x foo a.txt",
        "rg --heading=yes foo",
    ]);
}

// GNU tools take flags after the operands too (review): every word up to
// `--` is checked.
#[test]
fn flags_after_operands_are_checked() {
    assert_runs(&["grep foo src -r", "ls src -la", "sort a.txt -n"]);
    assert_asks(&[
        "grep foo . -R",
        "ls src -L",
        "sort a.txt --files0-from=list",
        "rg foo src --follow",
        "tail a.txt -f",
    ]);
}

// After `--` only operands follow.
#[test]
fn operands_after_double_dash_run() {
    assert_runs(&[
        "ls -- src",
        "cat -- a.txt",
        "grep -n -- -foo a.txt",
        "rg -- -foo src",
        "cat -",
    ]);
}

// The stdin forms of the path-list flags need no list file: the list can
// come from an `echo` stage (review). The flags are off the list.
#[test]
fn stdin_path_lists_from_echo_ask() {
    assert_asks(&[
        "echo a.txt | sort --files0-from=-",
        "echo a.txt | wc --files0-from=-",
        "echo a.txt | file -f -",
        "echo a.txt | find . -files0-from -",
    ]);
}

// User decision 5 of 28.10: a secrets file named in the command asks; the
// P2 list decides what counts.
#[test]
fn secret_file_names_ask() {
    assert_asks(&[
        "cat .env",
        "cat .env.local",
        "cat .npmrc",
        "head .netrc",
        "cat .git/config",
        "ls .ssh",
        "cat id_rsa",
        "grep token .pypirc",
        "cat server.pem",
    ]);
    assert_runs(&[
        "cat .env.example",
        "cat src/credentials.rs",
        "grep -rn foo . --exclude-dir=.git",
    ]);
    let why = refusal("cat .npmrc").unwrap();
    assert!(why.contains("secret"), "{why}");
}

#[test]
fn redirections_and_background_ask() {
    assert_asks(&["cat a.txt > b.txt", "ls 2>&1", "sort < a.txt", "ls &", ""]);
}

// Card: the excluded commands keep their class in the panel; only the
// auto-run check refuses them.
#[test]
fn panel_class_is_unchanged_for_excluded_commands() {
    for cmd in [
        "git status",
        "node -v",
        "ps aux",
        "more a.txt",
        "Get-ChildItem src",
    ] {
        assert_eq!(classify(cmd).local_risk, LocalRisk::ReadOnly, "{cmd}");
        assert!(refusal(cmd).is_some(), "{cmd:?} may run unasked");
    }
}

// Ruling 49: the list splits and tokenises without reading backslash
// escapes, as the classifier does, while the shell reads them. Any
// backslash outside single quotes asks.
#[test]
fn backslash_outside_single_quotes_asks() {
    let why = refusal("grep a\\.b a.txt").expect("grep a\\.b may run unasked");
    assert!(why.contains("backslash"), "{why}");
    assert_runs(&["grep 'a\\.b' a.txt"]);
}
