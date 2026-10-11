//! A process's display name (D-65): what it runs, not only its executable.
//! The kernel's `comm` stops at 15 bytes, and an interpreter (`python`,
//! `node`, `java` ...) names every program it runs alike, so where `comm`
//! says too little the name comes from the command line:
//!
//! - an interpreter: the module it runs (`-m vllm.entrypoints.openai.api_server`
//!   → `vllm.api_server`, its first and last parts) or its script
//!   (`train_supervisor.py` → `train_supervisor`);
//! - a `comm` cut at 15 bytes: the executable's full name when it extends
//!   it (`rust-analyzer-p` → `rust-analyzer-proc-macro-srv`);
//! - a generic name (`main`, `server`, `train` ...): with the folder it
//!   belongs to, the nearest that is not generic itself (`ComfyUI/main`);
//! - a program the command line does not name (`python -c ...`): its
//!   parent's name, else the interpreter in its folder.
//!
//! Only that name leaves this module (SECURITY.md: the full command line is
//! never recorded). It is taken only where the interpreter's grammar puts
//! its program, so an option value or code never becomes a name: Python's
//! and Java's options are known in full; for the other interpreters an
//! argument right after an option may be its value, so it names the
//! program only when it is a script file. Names are cleaned of control
//! characters and kept to [`MAX_NAME_CHARS`] characters.

use std::fmt::Display;
use std::path::{Component, Path};

use dgxtop_core::text::sanitize_command_name;

use crate::common::SystemFiles;

/// Longest display name, in characters.
pub(crate) const MAX_NAME_CHARS: usize = 24;
/// Shortest the folder of a long name is cut to.
const MIN_FOLDER_CHARS: usize = 6;
/// `comm` is cut to this many bytes (TASK_COMM_LEN − 1).
const COMM_LIMIT: usize = 15;
/// Thread names runtimes give a program's main thread, which `comm` shows
/// instead of the program (PyTorch, Python, Node).
const THREAD_NAMES: [&str; 3] = ["pt_main_thread", "MainThread", "node-MainThread"];

/// Program names that say nothing without their folder.
const GENERIC_NAMES: [&str; 34] = [
    "main",
    "app",
    "run",
    "runner",
    "server",
    "serve",
    "start",
    "index",
    "cli",
    "__main__",
    "launch",
    "launcher",
    "worker",
    "entrypoint",
    "entry",
    "wsgi",
    "asgi",
    "manage",
    "script",
    "test",
    "demo",
    "bot",
    "api",
    "web",
    "webui",
    "service",
    "train",
    "training",
    "finetune",
    "infer",
    "inference",
    "predict",
    "eval",
    "jupyter",
];
/// Folders a name never takes: build and layout folders, not projects.
const GENERIC_FOLDERS: [&str; 34] = [
    "bin",
    "sbin",
    "lib",
    "lib64",
    "src",
    "scripts",
    "script",
    "tools",
    "tool",
    "examples",
    "example",
    "app",
    "apps",
    "backend",
    "frontend",
    "server",
    "client",
    "cli",
    "cmd",
    "python",
    "py",
    "code",
    "tests",
    "test",
    "notebooks",
    "notebook",
    "jobs",
    "tasks",
    ".venv",
    "venv",
    "node_modules",
    "site-packages",
    "dist-packages",
    "workspace",
];
/// Top-level system folders.
const SYSTEM_FOLDERS: [&str; 8] = ["usr", "opt", "srv", "mnt", "var", "tmp", "root", "home"];
/// Modules that stand for something better known.
const MODULE_ALIASES: [(&str, &str); 2] =
    [("ipykernel_launcher", "jupyter"), ("ipykernel", "jupyter")];
/// Extensions a script name drops.
const SCRIPT_EXTENSIONS: [&str; 22] = [
    "py", "pyw", "js", "mjs", "cjs", "ts", "mts", "jsx", "tsx", "rb", "pl", "php", "sh", "bash",
    "zsh", "fish", "jl", "lua", "r", "jar", "java", "dll",
];

/// What a command line names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Named {
    Name(String),
    /// No program named (`python -c ...`): the parent's name when it has
    /// one, else `fallback`.
    Inherit {
        fallback: String,
    },
}

/// The arguments of process `pid`'s command line; `None` when it cannot
/// be read (the process is gone or another user's).
pub(crate) fn read_argv(files: &dyn SystemFiles, pid: impl Display) -> Option<Vec<String>> {
    let text = files.read_to_string(&format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        text.split('\0')
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// The command line as shown (D-73): the arguments joined by spaces, cleaned
/// of control characters, cut with `…` to `max_bytes`; empty when there is
/// none (kernel threads).
pub(crate) fn command_text(argv: &[String], max_bytes: usize) -> String {
    let joined = argv.join(" ");
    let cleaned = dgxtop_core::text::sanitize_display(&joined, usize::MAX);
    if cleaned.len() <= max_bytes {
        return cleaned;
    }
    let mut end = max_bytes.saturating_sub('…'.len_utf8());
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &cleaned[..end])
}

/// Whether `comm` alone may say too little, so the command line is read:
/// an interpreter, a runtime's thread name, or a name the kernel cut.
pub(crate) fn needs_command_line(comm: &str) -> bool {
    family(comm).is_some() || THREAD_NAMES.contains(&comm) || comm.len() >= COMM_LIMIT
}

/// The display name of a process (module docs). `argv` is its command
/// line, empty when unreadable (kernel threads have none); `cwd` gives its
/// working folder, asked only when a generic name needs a folder.
pub(crate) fn name_process(
    comm: &str,
    argv: &[String],
    cwd: impl FnOnce() -> Option<String>,
) -> Named {
    let comm = sanitize_command_name(comm);
    let Some(first) = argv.first() else {
        return Named::Name(comm);
    };
    let executable = executable_name(first);
    let Some(family) = family(&comm).or_else(|| family(&executable)) else {
        return Named::Name(cap(&full_comm(&comm, &executable), ""));
    };
    let mut cwd = Folder::lazy(cwd);
    match program(family, &argv[1..]) {
        Some(Program::Module(module)) => Named::Name(name_in_folder(&module, None, &mut cwd)),
        Some(Program::Script(path)) => {
            Named::Name(name_in_folder(&script_name(&path), Some(&path), &mut cwd))
        }
        Some(Program::Class(class)) => Named::Name(cap(&class, "")),
        None => Named::Inherit {
            fallback: in_folder(&interpreter_label(&executable, &comm), None, &mut cwd),
        },
    }
}

/// Interpreters whose argument grammar names their program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Python,
    Node,
    Java,
    Shell,
    /// Other script interpreters: the program is a path or a file with a
    /// script extension.
    Script,
}

fn family(name: &str) -> Option<Family> {
    let name = name.trim_start_matches('-');
    let versioned = |prefix: &str| {
        name.strip_prefix(prefix).is_some_and(|rest| {
            rest == "w"
                || rest
                    .chars()
                    .all(|character| character.is_ascii_digit() || character == '.')
        })
    };
    if versioned("python") || versioned("pypy") {
        return Some(Family::Python);
    }
    match name {
        "node" | "nodejs" | "deno" | "bun" => Some(Family::Node),
        "java" => Some(Family::Java),
        "bash" | "sh" | "dash" | "zsh" | "ksh" | "fish" | "ash" => Some(Family::Shell),
        "ruby" | "perl" | "php" | "Rscript" | "julia" | "lua" | "luajit" | "dotnet" | "mono" => {
            Some(Family::Script)
        }
        _ => None,
    }
}

/// What an interpreter runs.
enum Program {
    Module(String),
    Script(String),
    Class(String),
}

fn program(family: Family, args: &[String]) -> Option<Program> {
    match family {
        Family::Python => python_program(args),
        Family::Node => node_program(args),
        Family::Java => java_program(args),
        Family::Shell => shell_program(args),
        Family::Script => script_program(args),
    }
}

/// Python's options are fixed: single-letter flags (combinable, `-uB`),
/// `-X`/`-W`/`--check-hash-based-pycs` take a value, `-c` runs code and
/// `-m` a module; the first other argument is the script.
fn python_program(args: &[String]) -> Option<Program> {
    let mut rest = args.iter();
    while let Some(argument) = rest.next() {
        if argument == "--check-hash-based-pycs" {
            rest.next();
            continue;
        }
        if argument.starts_with("--") {
            continue;
        }
        if argument == "-" {
            // The program comes from standard input.
            return None;
        }
        let Some(flags) = argument.strip_prefix('-') else {
            return Some(Program::Script(argument.clone()));
        };
        for (position, flag) in flags.char_indices() {
            let value = &flags[position + flag.len_utf8()..];
            match flag {
                'c' => return None,
                'm' => {
                    let module = if value.is_empty() {
                        rest.next()?
                    } else {
                        value
                    };
                    return Some(Program::Module(module_name(module)));
                }
                'X' | 'W' => {
                    if value.is_empty() {
                        rest.next();
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    None
}

/// Node, Deno and Bun: `-e`/`-p` run code, the options listed take a value,
/// and the script is the first other argument (after Deno's and Bun's
/// `run`).
fn node_program(args: &[String]) -> Option<Program> {
    const WITH_VALUE: [&str; 15] = [
        "-r",
        "--require",
        "--loader",
        "--experimental-loader",
        "--import",
        "--input-type",
        "--env-file",
        "-C",
        "--conditions",
        "--run",
        "--title",
        "--disable-warning",
        "--watch-path",
        "--redirect-warnings",
        "--report-dir",
    ];
    let args = match args.split_first() {
        Some((first, rest)) if first == "run" => rest,
        _ => args,
    };
    let mut rest = args.iter();
    let mut after_option = false;
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "-e" | "--eval" | "-p" | "--print" => return None,
            option if WITH_VALUE.contains(&option) => {
                rest.next();
                after_option = false;
            }
            option if option.starts_with('-') => after_option = !option.contains('='),
            operand => return script_operand(operand, after_option),
        }
    }
    None
}

/// Java: the launcher's options are known in full. Those listed take a
/// value, `@file` reads options from a file, `-jar` names the archive and
/// `-m` the module; otherwise the first other argument is the main class
/// or a source file.
fn java_program(args: &[String]) -> Option<Program> {
    const WITH_VALUE: [&str; 16] = [
        "-cp",
        "-classpath",
        "--class-path",
        "-p",
        "--module-path",
        "--upgrade-module-path",
        "--add-modules",
        "--enable-native-access",
        "--limit-modules",
        "--add-reads",
        "--add-exports",
        "--add-opens",
        "--patch-module",
        "--source",
        "-d",
        "--describe-module",
    ];
    let mut rest = args.iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "-jar" => return rest.next().map(|jar| Program::Script(jar.clone())),
            "-m" | "--module" => {
                let module = rest.next()?;
                let main = module.split('/').next_back().unwrap_or(module);
                return Some(Program::Class(last_part(main).to_owned()));
            }
            option if WITH_VALUE.contains(&option) => {
                rest.next();
            }
            option if option.starts_with('-') || option.starts_with('@') => {}
            source if source.ends_with(".java") => return Some(Program::Script(source.to_owned())),
            class => return Some(Program::Class(last_part(class).to_owned())),
        }
    }
    None
}

/// Shells: `c` runs code and `s` reads it from standard input, alone or
/// among other letters (`-xc`); `o`/`O` (last of their letters) and
/// `--rcfile`/`--init-file` take a value; the program is the first other
/// argument.
fn shell_program(args: &[String]) -> Option<Program> {
    const WITH_VALUE: [&str; 2] = ["--rcfile", "--init-file"];
    let mut rest = args.iter();
    let mut after_option = false;
    while let Some(argument) = rest.next() {
        if WITH_VALUE.contains(&argument.as_str()) {
            rest.next();
            after_option = false;
            continue;
        }
        if argument.starts_with("--") {
            after_option = !argument.contains('=');
            continue;
        }
        let Some(letters) = argument
            .strip_prefix('-')
            .or_else(|| argument.strip_prefix('+'))
        else {
            return script_operand(argument, after_option);
        };
        if letters.contains(['c', 's']) {
            return None;
        }
        after_option = !letters.ends_with(['o', 'O']);
        if !after_option {
            rest.next();
        }
    }
    None
}

/// Other interpreters: `-c`, `-e`, `-E` and `-r` run code (or need what
/// follows them); the program is the first other argument.
fn script_program(args: &[String]) -> Option<Program> {
    let mut after_option = false;
    for argument in args {
        match argument.as_str() {
            "-c" | "-e" | "-E" | "-r" => return None,
            option if option.starts_with('-') || option.starts_with('+') => {
                after_option = !option.contains('=');
            }
            operand => return script_operand(operand, after_option),
        }
    }
    None
}

/// `argument` as the program: a path or a script file, but only a script
/// file right after an option, whose value it may be.
fn script_operand(argument: &str, after_option: bool) -> Option<Program> {
    let program = if after_option {
        extension_of(argument).is_some()
    } else {
        argument.contains('/') || extension_of(argument).is_some()
    };
    program.then(|| Program::Script(argument.to_owned()))
}

fn extension_of(name: &str) -> Option<&str> {
    let (_, extension) = name.rsplit_once('.')?;
    SCRIPT_EXTENSIONS
        .iter()
        .find(|known| known.eq_ignore_ascii_case(extension))
        .copied()
}

/// `vllm.entrypoints.openai.api_server` → `vllm.api_server`: the package
/// and the entry, without a trailing `__main__`; known modules by their
/// better known names.
fn module_name(module: &str) -> String {
    let parts: Vec<&str> = module.split('.').filter(|part| !part.is_empty()).collect();
    let parts = match parts.split_last() {
        Some((&"__main__", rest)) if !rest.is_empty() => rest,
        _ => parts.as_slice(),
    };
    let name = match parts {
        [] => module.to_owned(),
        [first, .., last] if parts.len() > 2 => format!("{first}.{last}"),
        _ => parts.join("."),
    };
    MODULE_ALIASES
        .iter()
        .find(|(module, _)| *module == name)
        .map_or(name, |(_, alias)| (*alias).to_owned())
}

/// The script's file name without its directory and script extension.
fn script_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    match extension_of(base) {
        Some(extension) => base[..base.len() - extension.len() - 1].to_owned(),
        None => base.to_owned(),
    }
}

/// `com.example.Trainer` → `Trainer`.
fn last_part(class: &str) -> &str {
    class.rsplit('.').next().unwrap_or(class)
}

/// The executable's name: the first word of `argv[0]` without its
/// directory (a title set by the program may hold spaces).
fn executable_name(first: &str) -> String {
    let word = first.split(' ').next().unwrap_or(first);
    sanitize_command_name(word.rsplit('/').next().unwrap_or(word))
}

/// `comm` cut by the kernel, or the executable's name it begins.
fn full_comm(comm: &str, executable: &str) -> String {
    if comm.len() >= COMM_LIMIT && executable.len() > comm.len() && executable.starts_with(comm) {
        executable.to_owned()
    } else {
        comm.to_owned()
    }
}

/// `python`, not `python3.12` or the thread name `pt_main_thread`.
fn interpreter_label(executable: &str, comm: &str) -> String {
    match family(executable).or_else(|| family(comm)) {
        Some(Family::Python) => "python".to_owned(),
        Some(_) if family(executable).is_some() => executable.trim_start_matches('-').to_owned(),
        _ => comm.trim_start_matches('-').to_owned(),
    }
}

/// `name`, with its folder when it is generic.
fn name_in_folder(
    name: &str,
    script: Option<&str>,
    cwd: &mut Folder<impl FnOnce() -> Option<String>>,
) -> String {
    let name = sanitize_command_name(name);
    if name == "__main__"
        && let Some(folder) = folder_of(script, cwd)
    {
        return cap(&folder, "");
    }
    if GENERIC_NAMES.contains(&name.as_str()) {
        in_folder(&name, script, cwd)
    } else {
        cap(&name, "")
    }
}

/// `folder/name` when a folder is found, else `name`.
fn in_folder(
    name: &str,
    script: Option<&str>,
    cwd: &mut Folder<impl FnOnce() -> Option<String>>,
) -> String {
    match folder_of(script, cwd) {
        Some(folder) => cap(name, &folder),
        None => cap(name, ""),
    }
}

/// The nearest folder of the script (or of the working folder for a
/// script given without one) that is not generic, a system folder or a
/// home folder.
fn folder_of(
    script: Option<&str>,
    cwd: &mut Folder<impl FnOnce() -> Option<String>>,
) -> Option<String> {
    let script_folder = script
        .and_then(|path| path.rsplit_once('/'))
        .map(|(folder, _)| folder.to_owned());
    let path = match script_folder {
        Some(folder) if folder.starts_with('/') => folder,
        Some(folder) => format!("{}/{folder}", cwd.get()?),
        None => cwd.get()?,
    };
    let parts: Vec<String> = Path::new(&path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str().map(str::to_owned),
            _ => None,
        })
        .collect();
    let home = parts
        .first()
        .is_some_and(|first| first == "home")
        .then_some(1);
    parts
        .iter()
        .enumerate()
        .rev()
        .filter(|(index, _)| Some(*index) != home)
        .map(|(_, part)| part)
        .find(|part| {
            !GENERIC_FOLDERS.contains(&part.as_str())
                && !SYSTEM_FOLDERS.contains(&part.as_str())
                && !part.starts_with('.')
        })
        .map(|part| sanitize_command_name(part))
}

/// `folder/name` within [`MAX_NAME_CHARS`]: the folder gives way first, to
/// [`MIN_FOLDER_CHARS`], then the name.
fn cap(name: &str, folder: &str) -> String {
    let name_chars = name.chars().count();
    if folder.is_empty() {
        return shorten(name, MAX_NAME_CHARS);
    }
    let folder_room = MAX_NAME_CHARS
        .saturating_sub(name_chars + 1)
        .max(MIN_FOLDER_CHARS);
    let folder = shorten(folder, folder_room);
    let name_room = MAX_NAME_CHARS.saturating_sub(folder.chars().count() + 1);
    format!("{folder}/{}", shorten(name, name_room))
}

/// `text` within `limit` characters, cut with `…`.
fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// The working folder, read once and only when asked.
struct Folder<F: FnOnce() -> Option<String>> {
    read: Option<F>,
    value: Option<String>,
}

impl<F: FnOnce() -> Option<String>> Folder<F> {
    fn lazy(read: F) -> Self {
        Self {
            read: Some(read),
            value: None,
        }
    }

    fn get(&mut self) -> Option<String> {
        if let Some(read) = self.read.take() {
            self.value = read();
        }
        self.value.clone()
    }
}

#[cfg(test)]
mod tests;
