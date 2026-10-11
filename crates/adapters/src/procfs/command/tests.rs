use super::*;

const PROJECT: &str = "/home/u/code/acme-llm";

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

fn named(comm: &str, words: &[&str], cwd: Option<&str>) -> Named {
    name_process(comm, &argv(words), || cwd.map(str::to_owned))
}

fn name(comm: &str, words: &[&str], cwd: Option<&str>) -> String {
    match named(comm, words, cwd) {
        Named::Name(name) => name,
        Named::Inherit { fallback } => panic!("{words:?} names no program (fallback {fallback})"),
    }
}

/// Command lines as GPU jobs run them: modules by package and entry,
/// scripts and console scripts by their file names, names the kernel cut
/// by the executable's full name.
#[test]
fn interpreters_are_named_by_what_they_run() {
    let venv = "/home/u/code/acme-llm/backend/.venv/bin";
    let python = format!("{venv}/python");
    let monitor = format!("{venv}/acme-monitor");
    let prepare = format!("{venv}/acme-prepare-data");
    let cases: [(&str, Vec<&str>, &str); 6] = [
        (
            "python",
            vec![
                &python,
                "-u",
                "-m",
                "acme_llm.training.train",
                "--run-id",
                "x",
            ],
            "acme_llm.train",
        ),
        (
            "python",
            vec!["python", "train_supervisor.py"],
            "train_supervisor",
        ),
        (
            "python",
            vec![
                "python",
                "backend/scripts/finetune_long_context_model.py",
                "--resume",
            ],
            "finetune_long_context_m…",
        ),
        (
            "python",
            vec!["python", &monitor, "--metrics", "m.jsonl"],
            "acme-monitor",
        ),
        (
            "acme-prepare-da",
            vec!["python", &prepare],
            "acme-prepare-data",
        ),
        (
            "pt_main_thread",
            vec!["python3", "-um", "vllm.entrypoints.openai.api_server"],
            "vllm.api_server",
        ),
    ];
    for (comm, words, expected) in cases {
        assert_eq!(name(comm, &words, Some(PROJECT)), expected, "{words:?}");
    }
    assert_eq!(
        name("acme-prepare-da", &[&prepare], None).chars().count(),
        17
    );
}

/// A generic program name takes the nearest folder that is a project, not
/// a layout folder, a system folder or the home folder.
#[test]
fn a_generic_name_takes_its_project_folder() {
    let comfy = "/home/u/ComfyUI";
    assert_eq!(
        name("python3", &["python3", "main.py", "--listen"], Some(comfy)),
        "ComfyUI/main"
    );
    // The script's own folder comes before the working folder; layout
    // folders are passed over.
    assert_eq!(
        name(
            "python",
            &["python", "/home/u/proj-a/scripts/train.py"],
            Some("/tmp")
        ),
        "proj-a/train"
    );
    assert_eq!(
        name(
            "node-MainThread",
            &["node", "index.mjs"],
            Some("/home/u/acme-llm/frontend")
        ),
        "acme-llm/index"
    );
    assert_eq!(
        name("python", &["python", "-m", "app"], Some("/srv/shop")),
        "shop/app"
    );
    // A package run as a script is named by its package.
    assert_eq!(
        name("python", &["python", "/opt/tool/mypkg/__main__.py"], None),
        "mypkg"
    );
    // Jupyter kernels by the notebook's folder.
    assert_eq!(
        name(
            "python",
            &["python", "-m", "ipykernel_launcher", "-f", "/x/k.json"],
            Some("/home/u/notebooks/llm")
        ),
        "llm/jupyter"
    );
    // No project folder to be found: the name alone.
    assert_eq!(
        name("python", &["python", "main.py"], Some("/home/u")),
        "main"
    );
    assert_eq!(name("python", &["python", "main.py"], None), "main");
    // A long folder gives way before the name.
    assert_eq!(
        name(
            "python",
            &["python", "server.py"],
            Some("/home/u/text-generation-webui")
        ),
        "text-generation-…/server"
    );
}

#[test]
fn node_java_and_shells_name_their_program() {
    assert_eq!(
        name(
            "node-MainThread",
            &["node", "/x/node_modules/nuxt/bin/nuxt.mjs", "dev"],
            None
        ),
        "nuxt"
    );
    assert_eq!(
        name("deno", &["deno", "run", "-A", "serve.ts"], None),
        "serve"
    );
    assert_eq!(
        name(
            "java",
            &["java", "-Xmx4g", "-cp", "lib/*", "com.example.Trainer"],
            None
        ),
        "Trainer"
    );
    assert_eq!(
        name("java", &["java", "-jar", "/opt/app/spark-runner.jar"], None),
        "spark-runner"
    );
    assert_eq!(
        name("bash", &["/bin/bash", "./run_all.sh"], None),
        "run_all"
    );
}

/// Names the kernel cut are completed by the executable; others stay.
#[test]
fn other_programs_keep_their_command_name() {
    let rust_analyzer = "/home/u/.rustup/toolchains/x/libexec/rust-analyzer-proc-macro-srv";
    assert_eq!(
        name("rust-analyzer-p", &[rust_analyzer], None),
        "rust-analyzer-proc-macr…"
    );
    assert_eq!(
        name("VLLM::EngineCor", &["VLLM::EngineCore"], None),
        "VLLM::EngineCore"
    );
    assert_eq!(
        name(
            "ollama",
            &["/usr/bin/ollama", "runner", "--model", "/m"],
            None
        ),
        "ollama"
    );
    // A title set over argv[0] keeps its first word only.
    assert_eq!(
        name("sshd", &["sshd -D [listener] 0 of 10-100 startups"], None),
        "sshd"
    );
    // Kernel threads have no command line.
    assert_eq!(name("kworker/0:1H", &[], None), "kworker/0:1H");
    assert!(!needs_command_line("ollama"));
    assert!(needs_command_line("python3.12") && needs_command_line("pt_main_thread"));
    assert!(needs_command_line("acme-prepare-da"));
}

/// Code given on the command line names no program: the parent's name is
/// used when it has one, else the interpreter in its folder.
#[test]
fn code_on_the_command_line_inherits_its_parents_name() {
    let spawn = [
        "python",
        "-c",
        "from multiprocessing.spawn import spawn_main; spawn_main(tracker_fd=5)",
        "--multiprocessing-fork",
    ];
    assert_eq!(
        named("python", &spawn, Some(PROJECT)),
        Named::Inherit {
            fallback: "acme-llm/python".to_owned()
        }
    );
    assert_eq!(
        named("python3.12", &["python3.12"], None),
        Named::Inherit {
            fallback: "python".to_owned()
        }
    );
    assert!(matches!(
        named("bash", &["bash", "-c", "sleep 1"], None),
        Named::Inherit { .. }
    ));
    assert!(matches!(
        named("node", &["node", "-e", "1"], None),
        Named::Inherit { .. }
    ));
    assert!(matches!(
        named("python", &["python", "-"], None),
        Named::Inherit { .. }
    ));
}

/// An option value never becomes a name: Python's options are known, and
/// the other interpreters only take a path or a script file.
#[test]
fn option_values_never_become_a_name() {
    assert_eq!(
        name(
            "python",
            &["python", "-W", "ignore", "-X", "dev", "job.py"],
            None
        ),
        "job"
    );
    assert_eq!(
        name(
            "python",
            &["python", "--check-hash-based-pycs", "always", "job.py"],
            None
        ),
        "job"
    );
    for words in [
        vec!["node", "--title", "SECRET", "app"],
        vec!["ruby", "-I", "SECRET"],
        vec!["bash", "-o", "SECRET"],
    ] {
        let fallback = match named(words[0], &words, None) {
            Named::Name(name) => name,
            Named::Inherit { fallback } => fallback,
        };
        assert!(!fallback.contains("SECRET"), "{words:?} gave {fallback}");
    }
    // Control characters in a name are removed.
    assert_eq!(
        name("python", &["python", "evil\u{1b}[2Jname.py"], None),
        "evilname"
    );
}

/// Java's launcher options are known in full; for Node, shells and other
/// interpreters an argument right after an option may be its value, so it
/// names the program only as a script file.
#[test]
fn other_interpreters_skip_option_values() {
    let opens = "java.base/java.lang=ALL-UNNAMED";
    assert_eq!(
        name(
            "java",
            &["java", "--add-opens", opens, "-jar", "app.jar"],
            None
        ),
        "app"
    );
    assert_eq!(
        name(
            "java",
            &["java", "@/tmp/jvm.args", "com.example.Main"],
            None
        ),
        "Main"
    );
    assert_eq!(
        name("java", &["java", "--source", "21", "Trainer.java"], None),
        "Trainer"
    );
    assert_eq!(
        name(
            "node",
            &["node", "--title", "SECRET/a", "server.js"],
            Some("/srv/shop")
        ),
        "shop/server"
    );
    assert_eq!(
        name("node", &["node", "--inspect", "dist/app.js"], None),
        "app"
    );
    assert_eq!(
        name(
            "bash",
            &["bash", "--rcfile", "/home/u/.SECRET", "./run.sh"],
            None
        ),
        "run"
    );
    assert_eq!(
        name("bash", &["bash", "-o", "pipefail", "./run.sh"], None),
        "run"
    );
    assert_eq!(name("bash", &["bash", "-ex", "./run.sh"], None), "run");
    for words in [
        vec!["node", "--unknown-option", "ab/SECRET"],
        vec!["bash", "-Z", "a/SECRET"],
        vec!["bash", "-xc", "./SECRET/run.sh"],
        vec!["bash", "-s", "./SECRET.sh"],
        vec!["php", "-r", "echo 'a/SECRET.php';"],
        vec!["ruby", "-x", "a/SECRET"],
    ] {
        let fallback = match named(words[0], &words, None) {
            Named::Name(name) => name,
            Named::Inherit { fallback } => fallback,
        };
        assert!(!fallback.contains("SECRET"), "{words:?} gave {fallback}");
    }
}
