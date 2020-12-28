use crypto::digest::Digest;
use crypto::sha1::Sha1;
use duct::cmd;
use notify::{DebouncedEvent, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env::current_dir;
use std::fmt;
use std::fs;
use std::io::{LineWriter, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

mod checksum;
mod color;
mod error;

use crate::checksum::*;
use crate::color::ColorPicker;
use crate::error::{BuildLoopError, SanityCheckError};

fn main() -> Result<(), String> {
    let file_name = ".buildy.yml";
    let contents = fs::read_to_string(file_name)
        .map_err(|e| format!("Something went wrong reading {}: {}", file_name, e))?;

    let mut targets: HashMap<String, Target> = serde_yaml::from_str(&contents)
        .map_err(|e| format!("Invalid format for {}: {}", file_name, e))?;
    let mut colors = ColorPicker::new();

    let mut names = targets.keys().cloned().collect::<Vec<String>>();
    names.sort_by(|a, b| a.partial_cmp(b).unwrap());

    for name in names.iter() {
        let mut target = targets.get_mut(name).unwrap();
        target.name = Some(name);
        target.color = Some(colors.get(name));
    }

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

struct BuildResult<'a> {
    target: &'a str,
    state: BuildResultState,
}

#[derive(Debug)]
enum BuildResultState {
    Success,
    Fail,
    Skip,
}

pub enum RunSignal {
    Kill,
}

impl fmt::Display for RunSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunSignal::Kill => write!(f, "KILL"),
        }
    }
}

struct Builder<'a> {
    targets: HashMap<String, Target<'a>>,
}

impl<'a> Builder<'a> {
    fn new(targets: HashMap<String, Target<'a>>) -> Self {
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

    fn process_path_change(
        &'a self,
        path: PathBuf,
        working_dir: &str,
        has_changed_files: &mut Vec<(&'a str, &'a str)>,
    ) {
        let absolute_path = match path.to_str() {
            Some(s) => s,
            None => return,
        };

        // TODO: This won't work with symlinks.
        let relative_path = &absolute_path[working_dir.len() + 1..];

        for (target_name, target) in self.targets.iter() {
            if relative_path.ends_with('~') {
                // Exclude files ending in ~ (generally temporary files)
                // TODO: Make this configurable.
                continue;
            }
            for watch_path in target.watch_list.iter() {
                let new_item = (target_name.as_str(), watch_path.as_str());
                if relative_path.starts_with(watch_path) && !has_changed_files.contains(&new_item) {
                    has_changed_files.push(new_item);
                }
            }
        }
    }

    fn choose_build_targets(
        &'a self,
        built_targets: &mut HashSet<&'a str>,
        building: &mut HashSet<&'a str>,
        has_changed_files: &mut Vec<(&'a str, &'a str)>,
        to_build: &mut HashSet<&'a str>,
    ) {
        for (target_name, target) in self.targets.iter() {
            let dependencies_satisfied = target
                .depends_on
                .iter()
                .all(|dependency| built_targets.contains(dependency.as_str()));

            if !dependencies_satisfied {
                continue;
            }

            if building.contains(target_name.as_str()) {
                continue;
            }

            if built_targets.contains(target_name.as_str()) {
                if !target.run_options.incremental {
                    continue;
                }

                let mut has_matching_target = false;

                // We don't skip if target name is in the list
                // and it's not in the non incremental list.

                for (t, watch_path) in has_changed_files.iter() {
                    if *t == target_name
                        && !target
                            .run_options
                            .watch_path_options
                            .non_incremental_list
                            .contains(&watch_path.to_owned().to_owned())
                    {
                        has_matching_target = true;
                    }
                }

                if !has_matching_target {
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
            let mut has_changed_files = Vec::new();
            let mut built_targets = HashSet::new();
            let mut building = HashSet::new();

            let (tx, rx) = channel();
            let working_dir = current_dir().map_err(BuildLoopError::CwdIOError)?;
            let working_dir = working_dir.to_str().ok_or(BuildLoopError::CwdUtf8Error)?;

            let run_tx_channels: Arc<Mutex<HashMap<&str, Sender<RunSignal>>>> = Default::default();

            loop {
                match watcher_rx.try_recv() {
                    Ok(result) => match result {
                        DebouncedEvent::Create(path)
                        | DebouncedEvent::Write(path)
                        | DebouncedEvent::Chmod(path)
                        | DebouncedEvent::Remove(path) => {
                            self.process_path_change(path, working_dir, &mut has_changed_files)
                        }
                        DebouncedEvent::Rename(path_before, path_after) => {
                            self.process_path_change(
                                path_before,
                                working_dir,
                                &mut has_changed_files,
                            );
                            self.process_path_change(
                                path_after,
                                working_dir,
                                &mut has_changed_files,
                            );
                        }
                        DebouncedEvent::Error(e, path) => match path {
                            None => println!("Watcher error {}", e),
                            Some(path) => match path.to_str() {
                                None => println!("Watcher error {}", e),
                                Some(path) => println!("Watcher error at \"{}\": {}", path, e),
                            },
                        },
                        _ => {}
                    },
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
                    building.insert(target_to_build);

                    let mut i = 0;
                    while i != has_changed_files.len() {
                        if &has_changed_files[i].0 == target_to_build {
                            has_changed_files.remove(i);
                        } else {
                            i += 1;
                        }
                    }

                    let tx_clone = tx.clone();
                    let target = self.targets.get(*target_to_build).unwrap().clone();
                    scope.spawn(move |_| {
                        if let Err(e) = target.build(tx_clone) {
                            target.log(&e)
                        }
                    });
                }
                to_build.clear();

                match rx.try_recv() {
                    Ok(result) => {
                        self.parse_build_result(&result, &mut building, &mut built_targets)?;

                        let target = self.targets.get(result.target).unwrap().clone();

                        if !target.run_list.is_empty() {
                            // If already running, it was rebuilt, send a kill signal.
                            match run_tx_channels.lock().unwrap().get(&result.target) {
                                None => {}
                                Some(run_tx) => {
                                    if let BuildResultState::Skip = result.state {
                                        continue;
                                    };
                                    run_tx
                                        .send(RunSignal::Kill)
                                        .map_err(BuildLoopError::SendError)?;
                                }
                            }

                            let (run_tx, run_rx) = channel();

                            let run_tx_channels = run_tx_channels.clone();
                            target.log("Running");
                            scope.spawn(move |_| {
                                // Wait for it to be killed.
                                while run_tx_channels.lock().unwrap().contains_key(&result.target) {
                                    sleep(Duration::from_millis(10));
                                }
                                run_tx_channels
                                    .lock()
                                    .unwrap()
                                    .insert(&result.target, run_tx);
                                if let Err(e) = target.run(run_rx) {
                                    target.log(&e)
                                };
                                run_tx_channels.lock().unwrap().remove(&result.target);
                            });
                        }
                    }
                    Err(e) => {
                        if e != TryRecvError::Empty {
                            return Err(BuildLoopError::RecvError(e));
                        }
                    }
                }

                sleep(Duration::from_millis(10));
            }
        })
        .map_err(|_| BuildLoopError::UnspecifiedChannelError)
        .and_then(|r| r)?;
        Ok(())
    }

    fn setup_watcher(
        &self,
    ) -> Result<
        (
            RecommendedWatcher,
            std::sync::mpsc::Receiver<DebouncedEvent>,
        ),
        BuildLoopError,
    > {
        let (watcher_tx, watcher_rx) = channel();
        let mut watcher: RecommendedWatcher = Watcher::new(watcher_tx, Duration::from_secs(0))
            .map_err(BuildLoopError::WatcherSetupError)?;
        for target in self.targets.values() {
            for watch_path in target.watch_list.iter() {
                watcher
                    .watch(watch_path, RecursiveMode::Recursive)
                    .map_err(|e| BuildLoopError::WatcherPathError(watch_path, e))?;
            }
        }

        Ok((watcher, watcher_rx))
    }

    fn parse_build_result(
        &'a self,
        result: &BuildResult<'a>,
        building: &mut HashSet<&'a str>,
        built_targets: &mut HashSet<&'a str>,
    ) -> Result<(), BuildLoopError> {
        match result.state {
            BuildResultState::Success => {}
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

fn default_true() -> bool {
    true
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
struct RunOptions {
    #[serde(default = "default_true")]
    incremental: bool,
    watch_path_options: WatchPathOptions,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            incremental: true,
            watch_path_options: WatchPathOptions::default(),
        }
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
struct WatchPathOptions {
    #[serde(default, rename = "non_incremental")]
    non_incremental_list: Vec<String>,
}

impl Default for WatchPathOptions {
    fn default() -> Self {
        WatchPathOptions {
            non_incremental_list: Vec::new(),
        }
    }
}

struct StdoutColorPrefixWriter<'a> {
    color: Color,
    line_prefix: &'a str,
}

impl<'a> Write for StdoutColorPrefixWriter<'a> {
    fn write(&mut self, buffer: &[u8]) -> std::result::Result<usize, std::io::Error> {
        let contents = std::str::from_utf8(buffer).unwrap();
        let mut contents = contents
            .split('\n')
            .map(|line| format!("{}{}", self.line_prefix, line))
            .collect::<Vec<String>>();
        if contents.len() > 1 {
            contents.truncate(contents.len() - 1);
        }
        let contents = contents.join("\n");

        let mut stdout = StandardStream::stdout(ColorChoice::Always);
        stdout.set_color(ColorSpec::new().set_fg(Some(self.color)))?;
        writeln!(&mut stdout, "{}", contents)?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::result::Result<(), std::io::Error> {
        // Nothing to do, this is instantly flushed.
        Ok(())
    }
}

#[derive(Debug, PartialEq, Deserialize, Clone)]
struct Target<'a> {
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

    #[serde(skip_deserializing)]
    name: Option<&'a str>,
    #[serde(skip_deserializing)]
    color: Option<Color>,
}

impl<'a> Target<'a> {
    fn build(&self, tx: Sender<BuildResult<'a>>) -> Result<(), String> {
        let mut hasher = Sha1::new();

        let name = self.name.unwrap();

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
                })
                .map_err(|e| format!("Sender error: {}", e))?;
                return Ok(());
            }
            write_checksum(name, &watch_checksum)?;
        }

        self.log("Building");
        for command in self.build_list.iter() {
            if let Err(e) = self.run_command(command, None) {
                self.log(&format!("Error: {}", e));
                tx.send(BuildResult {
                    target: name,
                    state: BuildResultState::Fail,
                })
                .map_err(|e| format!("Sender error: {}", e))?;
                return Ok(());
            }
        }

        self.log("Built");
        tx.send(BuildResult {
            target: name,
            state: BuildResultState::Success,
        })
        .map_err(|e| format!("Sender error: {}", e))?;
        Ok(())
    }

    fn run(&self, rx: Receiver<RunSignal>) -> Result<(), String> {
        for command in self.run_list.iter() {
            self.run_command(command, Some(&rx))?;
        }
        Ok(())
    }

    fn run_command(
        &self,
        command: &'a str,
        kill_rx: Option<&Receiver<RunSignal>>,
    ) -> Result<(), String> {
        let handle = cmd!("sh", "-c", command)
            .stderr_to_stdout()
            .reader()
            .map_err(|e| format!("Failed to run command {}: {}", command, e))?;
        let reader = Arc::new(handle);
        let (tx, rx) = channel();
        let reader_clone = reader.clone();
        let command_clone = command.to_owned();
        let line_prefix = format!("{} | ", self.name.unwrap());
        let color = self.color.unwrap();

        std::thread::spawn(move || {
            let mut writer = LineWriter::new(StdoutColorPrefixWriter {
                color,
                line_prefix: &line_prefix,
            });
            let mut buffer = [0u8; 1024];
            let mut read = || -> Result<usize, String> {
                let n = (&*reader_clone)
                    .read(&mut buffer)
                    .map_err(|e| format!("Failed to run command {}: {}", command_clone, e))?;
                if n == 0 {
                    return Ok(0);
                }
                writer
                    .write(&buffer[0..n])
                    .map_err(|e| format!("Error writing output: {}", e))?;
                Ok(n)
            };
            loop {
                match read() {
                    Ok(n) => {
                        if n == 0 {
                            if tx.send(Ok(())).is_err() {};
                            break;
                        }
                    }
                    Err(e) => {
                        if tx.send(Err(e)).is_err() {};
                        break;
                    }
                }
                sleep(Duration::from_millis(10));
            }
        });
        loop {
            match rx.try_recv() {
                Ok(result) => return result,
                Err(e) => match e {
                    TryRecvError::Empty => {}
                    _ => return Err(format!("Receiver error: {}", e)),
                },
            }
            if kill_rx.is_none() {
                continue;
            }
            match kill_rx.unwrap().try_recv() {
                Ok(RunSignal::Kill) => {
                    let kill_command = "kill ".to_owned()
                        + &(&*reader)
                            .pids()
                            .iter()
                            .map(|x| format!("{}", x))
                            .collect::<Vec<String>>()
                            .join(" ");
                    if self.run_command(&kill_command, None).is_err() {};
                    break;
                }
                Err(e) => match e {
                    TryRecvError::Empty => {}
                    _ => return Err(format!("Receiver error: {}", e)),
                },
            }
            sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    fn log(&self, message: &str) {
        let line_prefix = &format!("{} | ", self.name.unwrap());
        let color = self.color.unwrap();
        let mut writer = StdoutColorPrefixWriter { color, line_prefix };
        writer.write_all(message.as_bytes()).unwrap();
    }
}
