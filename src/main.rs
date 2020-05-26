use crossbeam;
use std::sync::mpsc::{Receiver, Sender, SendError, TryRecvError, channel};
use crypto::digest::Digest;
use crypto::sha1::Sha1;
use duct::cmd;
use notify::{DebouncedEvent, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env::current_dir;
use std::fmt;
use std::fs;
use std::io::Write;
use std::thread::sleep;
use std::time::Duration;
use walkdir::WalkDir;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use duct::unix::HandleExt;

fn main() -> Result<(), String> {
    let file_name = ".buildy.yml";
    let contents = fs::read_to_string(file_name)
        .map_err(|e| format!("Something went wrong reading {}: {}", file_name, e))?;

    let targets: HashMap<String, Target> = serde_yaml::from_str(&contents)
        .map_err(|e| format!("Invalid format for {}: {}", file_name, e))?;
    let builder = Builder::new(targets);

    builder
        .sanity_check()
        .map_err(|e| format!("Failed sanity check: {}", e))?;
    builder
        .build_loop()
        .map_err(|e| format!("Build loop error: {}", e))?;
    // TODO: Detect cycles.
    Ok(())
}

enum SanityCheckError<'a> {
    DependencyNotFound(&'a str),
    DependencyLoop(Vec<&'a str>),
}

impl fmt::Display for SanityCheckError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SanityCheckError::DependencyNotFound(dependency) => {
                write!(f, "Dependency {} not found.", dependency)
            }
            SanityCheckError::DependencyLoop(dependencies) => {
                write!(f, "Dependency loop: [{}]", dependencies.join(", "))
            }
        }
    }
}

enum BuildLoopError<'a> {
    BuildFailed(&'a String),
    UnspecifiedChannelError,
    SendError(SendError<RunSignal>),
    RecvError(TryRecvError),
    WatcherSetupError(notify::Error),
    WatcherPathError(&'a String, notify::Error),
    CwdIOError(std::io::Error),
    CwdUtf8Error,
}

impl fmt::Display for BuildLoopError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildLoopError::BuildFailed(target) => write!(f, "Build failed for target {}", target),
            BuildLoopError::UnspecifiedChannelError => {
                write!(f, "Unknown channel parllelism failure")
            }
            BuildLoopError::SendError(send_err) => write!(
                f,
                "Failed to send run signal '{}' to running process",
                send_err.0
            ),
            BuildLoopError::RecvError(recv_err) => {
                write!(f, "Channel receive failure: {}", recv_err)
            }
            BuildLoopError::WatcherSetupError(notify_err) => {
                write!(f, "Watcher setup error: {}", notify_err)
            }
            BuildLoopError::WatcherPathError(path, notify_err) => {
                write!(f, "File watch error: {}: {}", path, notify_err)
            }
            BuildLoopError::CwdIOError(io_err) => {
                write!(f, "IO Error while getting current directory: {}", io_err)
            }
            BuildLoopError::CwdUtf8Error => write!(f, "Current directory was not valid utf-8"),
        }
    }
}

struct BuildResult<'a> {
    target: &'a String,
    state: BuildResultState,
    output: String,
}

#[derive(Debug)]
enum BuildResultState {
    Success,
    Fail,
    Skip,
}

enum RunSignal {
    Kill,
}

impl fmt::Display for RunSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunSignal::Kill => write!(f, "KILL"),
        }
    }
}

struct Builder {
    targets: HashMap<String, Target>,
}

impl Builder {
    fn new(targets: HashMap<String, Target>) -> Self {
        Builder { targets }
    }

    fn sanity_check(&self) -> Result<(), SanityCheckError> {
        for (target_name, target) in self.targets.iter() {
            for dependency in target.depends_on.iter() {
                if !self.targets.contains_key(dependency.as_str()) {
                    return Err(SanityCheckError::DependencyNotFound(dependency));
                }
                if target_name == dependency {
                    return Err(SanityCheckError::DependencyLoop(vec![target_name]));
                }
            }
        }
        Ok(())
    }

    fn process_path_change<'a>(
        &'a self,
        path: PathBuf,
        working_dir: &str,
        has_changed_files: &mut HashSet<&'a String>,
    ) {
        let absolute_path = match path.to_str() {
            Some(s) => s,
            None => return,
        };

        // TODO: This won't work with symlinks.
        let relative_path = &absolute_path[working_dir.len() + 1..];

        for (target_name, target) in self.targets.iter() {
            if relative_path.ends_with("~") {
                // Exclude files ending in ~ (generally temporary files)
                // TODO: Make this configurable.
                continue;
            }
            if target
                .watch_list
                .iter()
                .any(|watch_path| relative_path.starts_with(watch_path))
            {
                has_changed_files.insert(target_name);
            }
        }
    }

    fn choose_build_targets<'a>(
        &'a self,
        built_targets: &mut HashSet<&'a String>,
        building: &mut HashSet<&'a String>,
        has_changed_files: &mut HashSet<&'a String>,
        to_build: &mut HashSet<&'a String>,
    ) {
        for (target_name, target) in self.targets.iter() {
            let dependencies_satisfied = target
                .depends_on
                .iter()
                .all(|dependency| built_targets.contains(dependency));

            if !dependencies_satisfied {
                continue;
            }
            if building.contains(target_name) {
                continue;
            }
            if built_targets.contains(target_name) {
                if !target.run_options.incremental {
                    continue;
                }
                if !has_changed_files.contains(target_name) {
                    continue;
                }
            }

            to_build.insert(target_name);
        }
    }

    fn build_loop(&self) -> Result<(), BuildLoopError> {
        /* Choose build targets (based on what's already been built, dependency tree, etc)
        Build all of them in parallel
        Wait for things to be built
        As things get built, check to see if there's something new we can build
        If so, start building that in parallel too

        Stop when nothing is still building and there's nothing left to build */
        crossbeam::scope(|scope| {
            let (_watcher, watcher_rx) = self.setup_watcher()?;

            let mut to_build = HashSet::new();
            let mut has_changed_files = HashSet::new();
            let mut built_targets = HashSet::new();
            let mut building = HashSet::new();

            let (tx, rx) = channel();
            let working_dir = current_dir().map_err(BuildLoopError::CwdIOError)?;
            let working_dir = working_dir
                .to_str()
                .ok_or_else(|| BuildLoopError::CwdUtf8Error)?;

            let run_tx_channels: Arc<Mutex<HashMap<&String, Sender<RunSignal>>>> = Default::default();

            loop {
                match watcher_rx.try_recv() {
                    Ok(result) => {
                        match result {
                            DebouncedEvent::Create(path) |
                            DebouncedEvent::Write(path) |
                            DebouncedEvent::Chmod(path) |
                            DebouncedEvent::Remove(path) => {
                                self.process_path_change(path, working_dir, &mut has_changed_files)
                            }
                            DebouncedEvent::Rename(path_before, path_after) => {
                                self.process_path_change(path_before, working_dir, &mut has_changed_files);
                                self.process_path_change(path_after, working_dir, &mut has_changed_files);
                            }
                            DebouncedEvent::Error(e, path) => match path {
                                None => println!("Watcher error {}", e),
                                Some(path) => match path.to_str() {
                                    None => println!("Watcher error {}", e),
                                    Some(path) => println!("Watcher error at \"{}\": {}", path, e)
                                },
                            },
                            _ => {}
                        }
                    }
                    Err(e) => match e {
                        TryRecvError::Empty => {}
                        _ => return Err(BuildLoopError::RecvError(e)),
                    },
                }

                self.choose_build_targets(
                    &mut built_targets,
                    &mut building,
                    &mut has_changed_files,
                    &mut to_build,
                );

                // if self.to_build.len() == 0 && self.building.len() == 0 {
                //    TODO: Exit if nothing to watch.
                //    break;
                // }

                for target_to_build in to_build.iter() {
                    let target_to_build = target_to_build.clone();
                    building.insert(target_to_build);
                    has_changed_files.remove(&target_to_build);
                    let tx_clone = tx.clone();
                    let target = self.targets.get(target_to_build).unwrap().clone();
                    scope.spawn(move |_| target.build(&target_to_build, tx_clone));
                }
                to_build.clear();

                match rx.try_recv() {
                    Ok(result) => {
                        self.parse_build_result(&result, &mut building, &mut built_targets)?;

                        let target = self.targets.get(result.target).unwrap().clone();

                        if !target.run_list.is_empty() {
                            // If already running, send a kill signal.
                            match run_tx_channels.lock().unwrap().get(result.target) {
                                None => {}
                                Some(run_tx) => run_tx
                                    .send(RunSignal::Kill)
                                    .map_err(BuildLoopError::SendError)?,
                            }

                            let (run_tx, run_rx) = channel();

                            let run_tx_channels = run_tx_channels.clone();
                            println!("Running {}", result.target);
                            scope.spawn(move |_| {
                                // Wait for it to be killed.
                                while run_tx_channels.lock().unwrap().contains_key(result.target) {
                                    sleep(Duration::from_millis(10));
                                }
                                run_tx_channels.lock().unwrap().insert(result.target, run_tx);
                                target.run(run_rx).unwrap();
                                run_tx_channels.lock().unwrap().remove(result.target);
                            });
                        }
                    }
                    Err(e) => {
                        if e != TryRecvError::Empty {
                            return Err(BuildLoopError::RecvError(e));
                        }
                    }
                }

                sleep(Duration::from_millis(10))
            }
        })
            .map_err(|_| BuildLoopError::UnspecifiedChannelError)
            .and_then(|r| r)?;
        Ok(())
    }

    fn setup_watcher(&self) -> Result<(RecommendedWatcher, std::sync::mpsc::Receiver<DebouncedEvent>), BuildLoopError> {
        let (watcher_tx, watcher_rx) = channel();
        let mut watcher: RecommendedWatcher =
            Watcher::new(watcher_tx, Duration::from_secs(0)).map_err(BuildLoopError::WatcherSetupError)?;
        for target in self.targets.values() {
            for watch_path in target.watch_list.iter() {
                watcher
                    .watch(watch_path, RecursiveMode::Recursive)
                    .map_err(|e| BuildLoopError::WatcherPathError(watch_path, e))?;
            }
        }

        Ok((watcher, watcher_rx))
    }

    fn parse_build_result<'a>(
        &'a self,
        result: &BuildResult<'a>,
        building: &mut HashSet<&'a String>,
        built_targets: &mut HashSet<&'a String>,
    ) -> Result<(), BuildLoopError> {
        match result.state {
            BuildResultState::Success => {
                if result.output != "" {
                    println!("Built {}:\n{}", result.target, result.output);
                }
            }
            BuildResultState::Fail => {
                return Err(BuildLoopError::BuildFailed(result.target));
            }
            BuildResultState::Skip => {}
        }
        building.remove(result.target);
        built_targets.insert(result.target);
        Ok(())
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
struct Target {
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default, rename = "watch")]
    watch_list: Vec<String>,
    #[serde(default, rename = "build")]
    build_list: Vec<String>,
    #[serde(default, rename = "run")]
    run_list: Vec<String>,
    #[serde(default)]
    run_options: RunOptions,
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
struct RunOptions {
    #[serde(default)]
    incremental: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions { incremental: true }
    }
}

impl Target {
    fn build<'a>(&self, name: &'a String, tx: Sender<BuildResult<'a>>) -> Result<(), String> {
        let mut output_string = String::from("");

        let mut hasher = Sha1::new();

        if !self.watch_list.is_empty() {
            for path in self.watch_list.iter() {
                let checksum = calculate_checksum(path)?;
                hasher.input_str(&checksum);
            }

            let watch_checksum = hasher.result_str();
            if does_checksum_match(name, &watch_checksum)? {
                tx.send(BuildResult {
                    target: name,
                    state: BuildResultState::Skip,
                    output: output_string,
                })
                  .map_err(|e| format!("Sender error: {}", e))?;
                return Ok(());
            }
            write_checksum(name, &watch_checksum)?;
        }

        println!("Building {}", name);
        for command in self.build_list.iter() {
            match cmd!("/bin/sh", "-c", command).stderr_to_stdout().run() {
                Ok(output) => {
                    output_string.push_str(
                        String::from_utf8(output.stdout)
                            .map_err(|e| format!("Failed to interpret stdout as utf-8: {}", e))?
                            .as_str(),
                    );
                }
                Err(e) => {
                    println!("Error running \"{}\": {}", e, command);
                    tx.send(BuildResult {
                        target: name,
                        state: BuildResultState::Fail,
                        output: output_string,
                    })
                      .map_err(|e| format!("Sender error: {}", e))?;
                    return Ok(());
                }
            }
        }

        tx.send(BuildResult {
            target: name,
            state: BuildResultState::Success,
            output: output_string,
        })
          .map_err(|e| format!("Sender error: {}", e))?;
        Ok(())
    }

    fn run(&self, rx: Receiver<RunSignal>) -> Result<(), String> {
        for command in self.run_list.iter() {
            let handle = cmd!("/bin/sh", "-c", command)
                .stderr_to_stdout()
                .start()
                .map_err(|e| format!("Failed to run command {}: {}", command, e))?;
            match rx.recv() {
                Ok(RunSignal::Kill) => {
                    let sigterm = 15;
                    return handle.send_signal(sigterm).map_err(|e| {
                        let result = format!("Failed to kill process {}: {}", command, e);
                        println!("{}", result);
                        result
                    });
                }
                Err(e) => return Err(format!("Receiver error: {}", e)),
            }
        }
        Ok(())
    }
}

fn calculate_checksum(path: &String) -> Result<String, String> {
    let mut hasher = Sha1::new();

    for entry in WalkDir::new(path) {
        let entry = entry.map_err(|e| format!("Failed to traverse directory: {}", e))?;

        if entry.path().is_file() {
            let entry_path = match entry.path().to_str() {
                Some(s) => s,
                None => return Err("Failed to convert file path into String".to_owned()),
            };
            let contents = fs::read(entry_path)
                .map_err(|e| format!("Failed to read file to calculate checksum: {}", e))?;
            hasher.input(contents.as_slice());
        }
    }

    Ok(hasher.result_str())
}

const CHECKSUM_DIRECTORY: &'static str = ".buildy";

fn checksum_file_name(target: &String) -> String {
    format!("{}/{}.checksum", CHECKSUM_DIRECTORY, target)
}

fn does_checksum_match(target: &String, checksum: &String) -> Result<bool, String> {
    // Might want to check for some errors like permission denied.
    fs::create_dir(CHECKSUM_DIRECTORY).ok();
    let file_name = checksum_file_name(target);
    match fs::read_to_string(&file_name) {
        Ok(old_checksum) => Ok(*checksum == old_checksum),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                // No checksum found.
                Ok(false)
            } else {
                Err(format!(
                    "Failed reading checksum file {} for target {}: {}",
                    file_name, target, e
                ))
            }
        }
    }
}

fn write_checksum(target: &String, checksum: &String) -> Result<(), String> {
    let file_name = checksum_file_name(target);
    let mut file = fs::File::create(&file_name).map_err(|_| {
        format!(
            "Failed to create checksum file {} for target {}",
            file_name, target
        )
    })?;
    file.write_all(checksum.as_bytes()).map_err(|_| {
        format!(
            "Failed to write checksum file {} for target {}",
            file_name, target
        )
    })?;
    Ok(())
}
