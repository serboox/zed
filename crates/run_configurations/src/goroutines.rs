use std::sync::Arc;
use std::time::Duration;

use gpui::SharedString;
use http_client::HttpClient;
use smol::io::AsyncReadExt as _;

/// Where a Go program's goroutines were read from. Nothing outside the program
/// can see them: they are the Go runtime's own, so only it can tell.
#[derive(Clone, Debug, PartialEq)]
pub enum GoroutineSource {
    /// The debugger the run is under, which lists them as its threads.
    Debugger,
    /// The program's own `net/http/pprof` endpoint, at this address.
    Pprof(SharedString),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Goroutines {
    pub total: usize,
    /// How many are in each state the runtime names (`running`, `IO wait`,
    /// `chan receive`, ...), largest first.
    pub by_state: Vec<(SharedString, usize)>,
    pub source: GoroutineSource,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GoroutineReading {
    Read(Goroutines),
    /// Why they cannot be shown, said as what the reader can do about it.
    Unavailable(SharedString),
}

/// How often the pprof endpoint is worth asking again. Fetching it walks
/// every goroutine's stack in the target process, so a poll as tight as the
/// process-metrics one would cost far more than the number is worth.
pub const POLL_INTERVAL: Duration = Duration::from_secs(4);

/// The first word of the commands this editor knows how to attach Delve to.
/// Kept in step with the `Delve` entry of `DEBUGGERS` in
/// `configurations_view.rs`, since the two answer the same question -- "is
/// this a Go program" -- from different information.
const GO_COMMANDS: &[&str] = &["go", "gotestsum", "dlv"];

/// A cheap guess at whether a run is a Go program, from the command it was
/// started with. Never a substitute for the debugger or pprof actually
/// answering -- just enough to decide whether to say anything at all when
/// neither of them is available.
pub fn looks_like_go_command(command: &str) -> bool {
    command
        .split_whitespace()
        .next()
        .is_some_and(|first| GO_COMMANDS.contains(&first))
}

/// Whether the program `pid` runs was built by the Go toolchain.
///
/// A run is often a script or a shell that starts the Go program, so the
/// command it was started with does not say. The binary does: every Go
/// executable carries a `.go.buildinfo` section. Only the ELF header and the
/// section table are read, not the program itself.
pub fn is_go_program(pid: u32) -> bool {
    let Ok(mut file) = std::fs::File::open(format!("/proc/{pid}/exe")) else {
        return false;
    };
    has_go_sections(&mut file).unwrap_or(false)
}

fn has_go_sections(file: &mut (impl std::io::Read + std::io::Seek)) -> std::io::Result<bool> {
    use std::io::SeekFrom;

    let mut header = [0u8; 64];
    file.read_exact(&mut header)?;
    // Only 64-bit little-endian ELF, which is every Linux machine this runs on.
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return Ok(false);
    }
    let u16_at = |bytes: &[u8], at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at = |bytes: &[u8], at: usize| {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    let u64_at = |bytes: &[u8], at: usize| {
        let mut word = [0u8; 8];
        word.copy_from_slice(&bytes[at..at + 8]);
        u64::from_le_bytes(word)
    };
    let table_at = u64_at(&header, 0x28);
    let entry_size = u16_at(&header, 0x3a) as usize;
    let count = u16_at(&header, 0x3c) as usize;
    let names_index = u16_at(&header, 0x3e) as usize;
    if entry_size < 0x28 || count == 0 || names_index >= count || count > 4096 {
        return Ok(false);
    }
    let mut table = vec![0u8; entry_size * count];
    file.seek(SeekFrom::Start(table_at))?;
    file.read_exact(&mut table)?;
    let entry = |index: usize| &table[index * entry_size..(index + 1) * entry_size];

    let names_entry = entry(names_index);
    let names_at = u64_at(names_entry, 0x18);
    let names_size = u64_at(names_entry, 0x20).min(1 << 20) as usize;
    let mut names = vec![0u8; names_size];
    file.seek(SeekFrom::Start(names_at))?;
    file.read_exact(&mut names)?;

    Ok((0..count).any(|index| {
        let name_at = u32_at(entry(index), 0) as usize;
        names
            .get(name_at..)
            .and_then(|rest| rest.split(|byte| *byte == 0).next())
            .is_some_and(|name| name == b".go.buildinfo")
    }))
}

/// What is said when a run looks like a Go program but neither a debugger nor
/// a pprof address is telling us its goroutines.
pub fn no_reader_configured() -> SharedString {
    "Run it under the debugger, or set a pprof address in this run \
     configuration (and import net/http/pprof) to see its goroutines."
        .into()
}

/// Reads every goroutine's state out of a `net/http/pprof`
/// `/debug/pprof/goroutine?debug=2` dump.
///
/// The dump is one paragraph per goroutine, starting with a line like
/// `goroutine 7 [IO wait, 2 minutes]:`. The state is the text inside the
/// brackets up to the first comma -- a comma there introduces a duration or a
/// "locked to thread" suffix, never more of the state itself.
pub fn parse_pprof_debug2(text: &str, source: GoroutineSource) -> Goroutines {
    let mut by_state: Vec<(SharedString, usize)> = Vec::new();
    let mut total = 0usize;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("goroutine ") else {
            continue;
        };
        let Some(open) = rest.find('[') else {
            continue;
        };
        let Some(close) = rest[open..].find(']') else {
            continue;
        };
        let inside = &rest[open + 1..open + close];
        let state = inside.split(',').next().unwrap_or(inside).trim();
        if state.is_empty() {
            continue;
        }
        total += 1;
        match by_state
            .iter_mut()
            .find(|(named, _)| named.as_ref() == state)
        {
            Some((_, count)) => *count += 1,
            None => by_state.push((state.into(), 1)),
        }
    }
    // Largest first, ties broken by name so two readings of the same dump
    // never reorder themselves for no reason a reader can see.
    by_state.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    Goroutines {
        total,
        by_state,
        source,
    }
}

/// The pprof endpoint's URL for `addr`, which may be a bare `host:port` or a
/// full URL.
fn pprof_url(addr: &str) -> String {
    let addr = addr.trim().trim_end_matches('/');
    let base = match addr.starts_with("http://") || addr.starts_with("https://") {
        true => addr.to_string(),
        false => format!("http://{addr}"),
    };
    format!("{base}/debug/pprof/goroutine?debug=2")
}

/// Fetches and parses `addr`'s goroutine dump. Runs entirely off the caller's
/// thread by virtue of `http_client` itself being asynchronous; the parsing
/// afterwards is cheap enough not to need its own background hop.
pub async fn read_pprof(http_client: Arc<dyn HttpClient>, addr: &str) -> GoroutineReading {
    let fetch = async {
        let mut response = http_client
            .get(&pprof_url(addr), Default::default(), true)
            .await?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;
        anyhow::ensure!(response.status().is_success(), "HTTP {}", response.status());
        Ok::<_, anyhow::Error>(String::from_utf8_lossy(&body).into_owned())
    };
    match fetch.await {
        Ok(text) => GoroutineReading::Read(parse_pprof_debug2(
            &text,
            GoroutineSource::Pprof(addr.into()),
        )),
        Err(error) => {
            GoroutineReading::Unavailable(format!("pprof at {addr} did not answer: {error}").into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The common case: a handful of goroutines, most idle in one state or
    /// another, one of them running.
    #[test]
    fn states_with_durations_are_counted_by_state_alone() {
        let dump = "\
goroutine 1 [running]:
main.main()

goroutine 2 [IO wait, 2 minutes]:
net.(*netFD).Read()

goroutine 3 [IO wait, 5 minutes]:
net.(*netFD).Read()

goroutine 4 [chan receive]:
main.worker()
";
        let read = parse_pprof_debug2(dump, GoroutineSource::Debugger);

        assert_eq!(read.total, 4);
        assert_eq!(
            read.by_state,
            vec![
                (SharedString::from("IO wait"), 2),
                (SharedString::from("chan receive"), 1),
                (SharedString::from("running"), 1),
            ],
            "IO wait has two goroutines regardless of how long each has been \
             waiting, and the busiest state leads"
        );
    }

    /// `locked to thread` is another comma-separated suffix, not a state of
    /// its own.
    #[test]
    fn a_goroutine_locked_to_its_thread_still_counts_by_state() {
        let dump = "goroutine 9 [syscall, locked to thread]:\nruntime.foo()\n";

        let read = parse_pprof_debug2(dump, GoroutineSource::Debugger);

        assert_eq!(read.total, 1);
        assert_eq!(read.by_state, vec![(SharedString::from("syscall"), 1)]);
    }

    /// A dump with nothing recognisable in it reports nothing, rather than
    /// panicking on a line that does not have the shape expected.
    #[test]
    fn an_empty_or_unrecognisable_dump_reports_no_goroutines() {
        assert_eq!(parse_pprof_debug2("", GoroutineSource::Debugger).total, 0);
        assert_eq!(
            parse_pprof_debug2("not a pprof dump at all\n", GoroutineSource::Debugger).total,
            0
        );
    }

    /// The url both a bare address and a full one should be fetched from.
    #[test]
    fn the_pprof_url_accepts_a_bare_address_or_a_full_one() {
        assert_eq!(
            pprof_url("localhost:6060"),
            "http://localhost:6060/debug/pprof/goroutine?debug=2"
        );
        assert_eq!(
            pprof_url("http://localhost:6060/"),
            "http://localhost:6060/debug/pprof/goroutine?debug=2"
        );
    }

    #[test]
    fn a_go_command_is_recognised_by_its_first_word() {
        assert!(looks_like_go_command("go run ./cmd/api"));
        assert!(looks_like_go_command("gotestsum --format testname"));
        assert!(!looks_like_go_command("cargo test"));
    }

    /// A 64-bit little-endian ELF with the given section names, laid out the
    /// way a linker lays it out: header, names, then the section table.
    fn an_elf_with_sections(section_names: &[&str]) -> Vec<u8> {
        let mut names = vec![0u8];
        let mut offsets = Vec::new();
        for name in section_names.iter().chain([&".shstrtab"]) {
            offsets.push(names.len() as u32);
            names.extend_from_slice(name.as_bytes());
            names.push(0);
        }
        let names_at = 64u64;
        let table_at = names_at + names.len() as u64;
        let count = offsets.len() as u16;
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[0x28..0x30].copy_from_slice(&table_at.to_le_bytes());
        elf[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        elf[0x3c..0x3e].copy_from_slice(&count.to_le_bytes());
        elf[0x3e..0x40].copy_from_slice(&(count - 1).to_le_bytes());
        elf.extend_from_slice(&names);
        for offset in offsets {
            let mut entry = vec![0u8; 64];
            entry[..4].copy_from_slice(&offset.to_le_bytes());
            entry[0x18..0x20].copy_from_slice(&names_at.to_le_bytes());
            entry[0x20..0x28].copy_from_slice(&(names.len() as u64).to_le_bytes());
            elf.extend_from_slice(&entry);
        }
        elf
    }

    #[test]
    fn a_go_binary_is_told_by_its_build_info_section() {
        let go = an_elf_with_sections(&[".text", ".go.buildinfo", ".data"]);
        let other = an_elf_with_sections(&[".text", ".data", ".go.buildinfo.old"]);
        assert!(has_go_sections(&mut std::io::Cursor::new(go)).expect("reads"));
        assert!(!has_go_sections(&mut std::io::Cursor::new(other)).expect("reads"));
        assert!(
            !has_go_sections(&mut std::io::Cursor::new(b"#!/bin/sh\n".repeat(10))).expect("reads"),
            "a script is not a Go program"
        );
    }

    /// The editor itself is not a Go program; a check that answered yes to
    /// everything would pass the test above for the wrong reason.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_running_test_is_not_a_go_program() {
        assert!(!is_go_program(std::process::id()));
    }

    #[test]
    fn the_message_when_nothing_is_configured_tells_the_reader_what_to_do() {
        let message = no_reader_configured();

        assert!(
            message.contains("debugger") && message.contains("pprof"),
            "both ways of seeing them have to be in it: {message}"
        );
    }
}
