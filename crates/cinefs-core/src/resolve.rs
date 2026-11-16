//! DaVinci Resolve Media Pool import via `fuscript` — shared by every front-end (macOS, Windows, CLI).
//!
//! The UI layers only decide *where* `fuscript` lives and what to show; building the Lua script,
//! running the process with a timeout and classifying its output lives here so all platforms
//! behave (and report errors) identically.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Result of an import attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// `n > 0` items were added to the Media Pool.
    Imported(usize),
    /// Imported, and an attempt was made to set the clip Pixel Aspect Ratio for stretched clips.
    ImportedPar { count: usize, par_applied: usize, par_total: usize },
    /// Resolve answered but added nothing (clips unreadable for Resolve, or already imported).
    ImportedNothing,
    /// Resolve is running but no project is open.
    NoProject,
    /// Resolve not running / external scripting disabled (Studio only).
    NoResolve,
    /// `fuscript` executable not found.
    NoFuscript,
    /// Anything else (message is human readable).
    Failed(String),
}

impl ResolveOutcome {
    /// Stable machine-readable tag (used by the C ABI / JSON).
    pub fn tag(&self) -> &'static str {
        match self {
            ResolveOutcome::Imported(_) | ResolveOutcome::ImportedPar { .. } => "imported",
            ResolveOutcome::ImportedNothing => "imported_nothing",
            ResolveOutcome::NoProject => "no_project",
            ResolveOutcome::NoResolve => "no_resolve",
            ResolveOutcome::NoFuscript => "no_fuscript",
            ResolveOutcome::Failed(_) => "failed",
        }
    }

    /// `{"status":"imported","count":3,"message":""}`
    pub fn to_json(&self) -> String {
        let (count, msg) = match self {
            ResolveOutcome::Imported(n) => (*n, String::new()),
            ResolveOutcome::ImportedPar { count, .. } => (*count, String::new()),
            ResolveOutcome::Failed(m) => (0, m.clone()),
            _ => (0, String::new()),
        };
        let (pa, pt) = match self {
            ResolveOutcome::ImportedPar { par_applied, par_total, .. } => (*par_applied, *par_total),
            _ => (0, 0),
        };
        format!(
            "{{\"status\":\"{}\",\"count\":{},\"message\":\"{}\",\"parApplied\":{},\"parTotal\":{}}}",
            self.tag(),
            count,
            crate::summary::escape_json_str(&msg),
            pa,
            pt
        )
    }
}

/// Pixel aspect ratio (pixel width / pixel height) wanted for a clip, matched by folder name.
#[derive(Clone, Debug)]
pub struct ClipPar {
    pub folder: String,
    pub par: f64,
}

fn lua_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Builds the Lua script. It prints status tokens: `OK:<n>`, `ERR_NO_RESOLVE`, `ERR_NO_PROJECT`,
/// `ERR_NO_MEDIASTORAGE`, `ERR_LUA:<message>`, `PAR:<applied>/<total>` and `DIAG ...` lines.
pub fn build_lua_script<S: AsRef<str>>(clip_paths: &[S]) -> String {
    build_lua_script_with_par(clip_paths, &[])
}

pub fn build_lua_script_with_par<S: AsRef<str>>(clip_paths: &[S], pars: &[ClipPar]) -> String {
    let items: Vec<String> = clip_paths.iter().map(|p| lua_str(p.as_ref())).collect();
    let par_items: Vec<String> = pars
        .iter()
        .map(|p| format!("{{{}, {:.6}}}", lua_str(&p.folder), p.par))
        .collect();
    const TEMPLATE: &str = "local ok, err = pcall(function()\n\
  local resolve = bmd.scriptapp(\"Resolve\")\n\
  if not resolve then print(\"ERR_NO_RESOLVE\") return end\n\
  local pm = resolve:GetProjectManager()\n\
  local proj = pm and pm:GetCurrentProject() or nil\n\
  if not proj then print(\"ERR_NO_PROJECT\") return end\n\
  local ms = resolve:GetMediaStorage()\n\
  if not ms then print(\"ERR_NO_MEDIASTORAGE\") return end\n\
  local items = ms:AddItemListToMediaPool({__ITEMS__})\n\
  resolve:OpenPage(\"media\")\n\
__PAR__\
  if items and #items > 0 then print(\"OK:\" .. tostring(#items)) else print(\"OK:0\") end\n\
end)\n\
if not ok then print(\"ERR_LUA:\" .. tostring(err)) end\n";
    // Resolve's `PAR` clip property only accepts its preset strings (numbers are rejected), so the
    // first stretched clip is used to PROBE which strings are accepted (and dump every property);
    // the original value is restored afterwards. Everything lands in the log as `DIAG` lines.
    const PAR_BLOCK: &str = "  local PARS = {__PARS__}\n\
  local par_total, par_applied, probed = 0, 0, false\n\
  if items then\n\
    for _, it in ipairs(items) do\n\
      local hay = tostring(it:GetClipProperty(\"File Path\") or \"\") .. \"|\" .. tostring(it:GetClipProperty(\"Clip Name\") or \"\")\n\
      for _, e in ipairs(PARS) do\n\
        if string.find(hay, e[1], 1, true) then\n\
          par_total = par_total + 1\n\
          if not probed then\n\
            probed = true\n\
            local all = it:GetClipProperty()\n\
            if type(all) == \"table\" then\n\
              for k, v in pairs(all) do print(\"DIAG prop \" .. tostring(k) .. \"=\" .. tostring(v)) end\n\
            end\n\
            local orig = tostring(it:GetClipProperty(\"PAR\"))\n\
            local cands = {\"Square\", \"Square Pixels\", \"16:9 Anamorphic\", \"16:9 SD\", \"4:3 SD\", \"4:3 Letterbox\", \"4:3 Pillarbox\", \"4:3 Standard\", \"Cinemascope\", \"Cinemascope 2x\", \"Anamorphic\", \"2x Anamorphic\", \"Vistavision\", \"1.33\", \"1.5\", \"2\", \"2.0\", \"3\", \"3.0\", \"1:3\", \"3:1\", \"Custom\", \"Project Settings\", \"NTSC 4:3\", \"PAL 4:3\", \"HDV 1080\"}\n\
            local accepted = {}\n\
            for _, c in ipairs(cands) do\n\
              local r = it:SetClipProperty(\"PAR\", c)\n\
              local after = tostring(it:GetClipProperty(\"PAR\"))\n\
              print(\"DIAG probe PAR try=\" .. c .. \" ret=\" .. tostring(r) .. \" after=\" .. after)\n\
              if r then accepted[#accepted + 1] = c .. \"->\" .. after end\n\
            end\n\
            print(\"DIAG probe accepted: \" .. table.concat(accepted, \" | \"))\n\
            it:SetClipProperty(\"PAR\", orig)\n\
            print(\"DIAG probe restored PAR=\" .. tostring(it:GetClipProperty(\"PAR\")))\n\
          end\n\
        end\n\
      end\n\
    end\n\
  end\n\
  print(\"PAR:\" .. par_applied .. \"/\" .. par_total)\n";
    let par_code = if pars.is_empty() {
        String::new()
    } else {
        PAR_BLOCK.replace("__PARS__", &par_items.join(", "))
    };
    TEMPLATE
        .replace("__ITEMS__", &items.join(", "))
        .replace("__PAR__", &par_code)
}

/// Classifies `fuscript` output.
pub fn parse_output(stdout: &str, stderr: &str, exit_code: Option<i32>) -> ResolveOutcome {
    if let Some(pos) = stdout.find("OK:") {
        let n: usize = stdout[pos + 3..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        if n == 0 {
            return ResolveOutcome::ImportedNothing;
        }
        if let Some(pp) = stdout.find("PAR:") {
            let tail = &stdout[pp + 4..];
            let a: usize = tail.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
            let t: usize = tail
                .split('/')
                .nth(1)
                .map(|x| x.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0))
                .unwrap_or(0);
            if t > 0 {
                return ResolveOutcome::ImportedPar { count: n, par_applied: a, par_total: t };
            }
        }
        return ResolveOutcome::Imported(n);
    }
    if stdout.contains("ERR_NO_PROJECT") {
        ResolveOutcome::NoProject
    } else if stdout.contains("ERR_NO_RESOLVE") {
        ResolveOutcome::NoResolve
    } else if let Some(pos) = stdout.find("ERR_") {
        ResolveOutcome::Failed(stdout[pos..].lines().next().unwrap_or("").to_string())
    } else {
        ResolveOutcome::Failed(format!(
            "no recognizable output from fuscript (exit {:?}) {}",
            exit_code,
            stderr.trim()
        ))
    }
}

fn run_once(fuscript: &Path, args: &[&std::ffi::OsStr], timeout: Duration) -> Result<(String, String, Option<i32>, bool), String> {
    let mut cmd = Command::new(fuscript);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    if let Some(dir) = fuscript.parent() {
        if dir.is_dir() {
            cmd.current_dir(dir);
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot start fuscript: {e}"))?;
    let t0 = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if t0.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }
    let status = child.wait().map_err(|e| format!("fuscript wait failed: {e}"))?;
    let mut so = String::new();
    let mut se = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut so);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut se);
    }
    crate::log::write(
        "INFO",
        &format!(
            "fuscript {:?} finished in {} ms, exit={:?}, timed_out={timed_out}\nstdout: {}\nstderr: {}",
            args,
            t0.elapsed().as_millis(),
            status.code(),
            so.trim(),
            se.trim()
        ),
    );
    Ok((so, se, status.code(), timed_out))
}

/// Runs the import: `fuscript -l lua <script file>`; if that yields nothing recognizable, retries
/// with `fuscript -l lua -x <script>` (older builds / macOS). Blocking, bounded by `timeout`.
pub fn run_import(fuscript: &Path, clips: &[PathBuf], timeout: Duration) -> ResolveOutcome {
    run_import_with_par(fuscript, clips, &[], timeout)
}

/// Like [`run_import`], additionally trying to set the clip Pixel Aspect Ratio for clips listed in `pars`.
pub fn run_import_with_par(
    fuscript: &Path,
    clips: &[PathBuf],
    pars: &[ClipPar],
    timeout: Duration,
) -> ResolveOutcome {
    if !fuscript.is_file() {
        crate::log::write("WARN", &format!("fuscript not found at '{}'", fuscript.display()));
        return ResolveOutcome::NoFuscript;
    }
    let paths: Vec<String> = clips.iter().map(|p| p.to_string_lossy().to_string()).collect();
    let lua = build_lua_script_with_par(&paths, pars);
    crate::log::write("INFO", &format!("Resolve import via '{}':\n{lua}", fuscript.display()));

    let script_path = std::env::temp_dir().join("cinefs_resolve_import.lua");
    let mut attempts: Vec<Vec<std::ffi::OsString>> = Vec::new();
    if fs::write(&script_path, &lua).is_ok() {
        attempts.push(vec!["-l".into(), "lua".into(), script_path.clone().into_os_string()]);
    }
    attempts.push(vec!["-l".into(), "lua".into(), "-x".into(), lua.clone().into()]);

    let mut last = ResolveOutcome::Failed(String::from("fuscript was not run"));
    for args in attempts {
        let refs: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_os_str()).collect();
        match run_once(fuscript, &refs, timeout) {
            Ok((_, _, _, true)) => {
                return ResolveOutcome::Failed(format!("fuscript timed out after {} s", timeout.as_secs()))
            }
            Ok((so, se, code, false)) => {
                let outcome = parse_output(&so, &se, code);
                if !matches!(outcome, ResolveOutcome::Failed(_)) {
                    return outcome;
                }
                last = outcome;
            }
            Err(e) => last = ResolveOutcome::Failed(e),
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_escapes_paths() {
        let lua = build_lua_script(&[r"Z:\clip", "a\"b"]);
        assert!(lua.contains(r#""Z:\\clip""#));
        assert!(lua.contains(r#""a\"b""#));
        assert!(lua.contains("AddItemListToMediaPool"));
    }

    #[test]
    fn classifies_output() {
        assert_eq!(parse_output("hello\nOK:3\n", "", Some(0)), ResolveOutcome::Imported(3));
        assert_eq!(parse_output("OK:0", "", Some(0)), ResolveOutcome::ImportedNothing);
        assert_eq!(parse_output("ERR_NO_PROJECT", "", Some(0)), ResolveOutcome::NoProject);
        assert_eq!(parse_output("ERR_NO_RESOLVE", "", Some(0)), ResolveOutcome::NoResolve);
        assert!(matches!(parse_output("ERR_LUA:boom", "", Some(0)), ResolveOutcome::Failed(m) if m.contains("boom")));
        assert!(matches!(parse_output("", "bad", Some(1)), ResolveOutcome::Failed(_)));
    }

    #[test]
    fn par_script_and_parse() {
        let pars = [ClipPar { folder: "clip_A".into(), par: 3.0 }];
        let lua = build_lua_script_with_par(&["/x/clip_A"], &pars);
        assert!(lua.contains("SetClipProperty"));
        assert!(lua.contains("\"clip_A\", 3.000000"));
        assert!(!build_lua_script(&["/x/clip_A"]).contains("SetClipProperty"));
        assert_eq!(
            parse_output("DIAG par x\nPAR:1/2\nOK:2\n", "", Some(0)),
            ResolveOutcome::ImportedPar { count: 2, par_applied: 1, par_total: 2 }
        );
    }

    /// Writes the generated Lua to `$CINEFS_DUMP_LUA` (if set) so it can be syntax-checked with `luac -p`.
    #[test]
    fn dump_lua_for_syntax_check() {
        if let Ok(path) = std::env::var("CINEFS_DUMP_LUA") {
            let pars = [ClipPar { folder: "clip_A".into(), par: 3.0 }];
            std::fs::write(&path, build_lua_script_with_par(&["/x/clip_A", "C:\\y\\z"], &pars)).unwrap();
            std::fs::write(format!("{path}.nopar"), build_lua_script(&["/x/clip_A"])).unwrap();
        }
    }
}
