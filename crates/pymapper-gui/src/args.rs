//! Command-line arguments. Hand-parsed: clap is reserved for the CLI by the dependency policy.

use std::ffi::OsString;
use std::path::PathBuf;

pub const USAGE: &str = "\
pymapper-gui: native code map for Python codebases

Usage: pymapper-gui [OPTIONS] [PATH]

Arguments:
  [PATH]             Project root to open (default: current directory)

Options:
      --measure      Measure frame time and idle CPU, print a report and exit
      --frames <N>   Frames to time in --measure mode [default: 300]
      --idle <SECS>  Idle window in --measure mode [default: 5]
  -h, --help         Print help
  -V, --version      Print version

Environment:
  WGPU_BACKEND       Override the GPU backend (metal on macOS, vulkan on Linux)
  WGPU_POWER_PREF    GPU preference: low (default) or high
";

/// Default number of timed frames in `--measure` mode.
pub const DEFAULT_FRAMES: u32 = 300;
/// Default idle window in `--measure` mode, in seconds.
pub const DEFAULT_IDLE_SECS: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasureOpts {
    pub frames: u32,
    pub idle_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub path: PathBuf,
    pub measure: Option<MeasureOpts>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Options),
    Help,
    Version,
}

/// Parses arguments (without the program name). Errors are usage messages (exit code 2).
pub fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Result<Parsed, String> {
    let mut path: Option<PathBuf> = None;
    let mut measure = false;
    let mut frames: Option<u32> = None;
    let mut idle: Option<u32> = None;
    let mut it = args.into_iter();
    let mut positional_only = false;
    while let Some(arg) = it.next() {
        if !positional_only && let Some(s) = arg.to_str() {
            match s {
                "-h" | "--help" => return Ok(Parsed::Help),
                "-V" | "--version" => return Ok(Parsed::Version),
                "--measure" => {
                    measure = true;
                    continue;
                }
                "--frames" => {
                    frames = Some(number(s, it.next())?);
                    continue;
                }
                "--idle" => {
                    idle = Some(number(s, it.next())?);
                    continue;
                }
                "--" => {
                    positional_only = true;
                    continue;
                }
                _ if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option '{s}'")),
                _ => {}
            }
        }
        if path.is_some() {
            return Err("at most one PATH may be given".to_owned());
        }
        path = Some(PathBuf::from(arg));
    }
    if !measure && (frames.is_some() || idle.is_some()) {
        return Err("--frames and --idle require --measure".to_owned());
    }
    let measure = measure.then(|| MeasureOpts {
        frames: frames.unwrap_or(DEFAULT_FRAMES),
        idle_secs: idle.unwrap_or(DEFAULT_IDLE_SECS),
    });
    Ok(Parsed::Run(Options { path: path.unwrap_or_else(|| PathBuf::from(".")), measure }))
}

fn number(flag: &str, value: Option<OsString>) -> Result<u32, String> {
    let value = value.ok_or_else(|| format!("{flag} requires a value"))?;
    let text = value.to_str().ok_or_else(|| format!("{flag}: value is not valid UTF-8"))?;
    match text.parse::<u32>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("{flag}: expected a positive integer, got '{text}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Parsed, String> {
        parse(args.iter().map(OsString::from))
    }

    fn opts(args: &[&str]) -> Options {
        match run(args) {
            Ok(Parsed::Run(o)) => o,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn defaults() {
        let o = opts(&[]);
        assert_eq!(o.path, PathBuf::from("."));
        assert_eq!(o.measure, None);
    }

    #[test]
    fn path_and_measure() {
        let o = opts(&["proj", "--measure"]);
        assert_eq!(o.path, PathBuf::from("proj"));
        assert_eq!(o.measure, Some(MeasureOpts { frames: DEFAULT_FRAMES, idle_secs: DEFAULT_IDLE_SECS }));
        let o = opts(&["--measure", "--frames", "10", "--idle", "2"]);
        assert_eq!(o.measure, Some(MeasureOpts { frames: 10, idle_secs: 2 }));
    }

    #[test]
    fn help_and_version_win() {
        assert_eq!(run(&["--measure", "-h"]), Ok(Parsed::Help));
        assert_eq!(run(&["-V"]), Ok(Parsed::Version));
    }

    #[test]
    fn errors() {
        assert!(run(&["--bogus"]).unwrap_err().contains("unknown option"));
        assert!(run(&["a", "b"]).unwrap_err().contains("at most one"));
        assert!(run(&["--measure", "--frames"]).unwrap_err().contains("requires a value"));
        assert!(run(&["--measure", "--frames", "0"]).unwrap_err().contains("positive"));
        assert!(run(&["--measure", "--idle", "x"]).unwrap_err().contains("positive"));
        assert!(run(&["--frames", "5"]).unwrap_err().contains("require --measure"));
    }

    #[test]
    fn double_dash_allows_dash_paths() {
        assert_eq!(opts(&["--", "-weird"]).path, PathBuf::from("-weird"));
        assert_eq!(opts(&["-"]).path, PathBuf::from("-"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_is_accepted() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(vec![b'p', 0xff]);
        match parse([raw.clone()]) {
            Ok(Parsed::Run(o)) => assert_eq!(o.path.as_os_str(), raw.as_os_str()),
            other => panic!("{other:?}"),
        }
    }
}
