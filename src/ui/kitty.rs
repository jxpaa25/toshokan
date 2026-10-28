//! Showing images with the kitty graphics protocol directly. Ghostty and kitty support it.
//!
//! Images are sent as raw RGB, preferably through a temporary file so only a ~100 byte escape
//! sequence goes through the terminal, and attached to a "virtual placement" of a given size in
//! cells. They're drawn by writing placeholder characters (U+10EEEE) whose foreground colour is
//! the image id; the terminal scales the image to fit the placement on the GPU. Showing another
//! page or scrolling then only changes which placeholder cells are on screen.
//!
//! Reference: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;
use ratatui_image::picker::{Picker, ProtocolType};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

const PLACEHOLDER: char = '\u{10EEEE}';
/// The image id used for the startup check; never used for an actual image.
const PROBE_ID: u32 = 31;
/// The protocol allows up to 4096 bytes of base64 per chunk.
const CHUNK: usize = 4096;
/// Ids must fit in the 24-bit foreground colour of the placeholder cells.
const MAX_ID: u32 = (1 << 24) - 1;

/// Ids below this are left to other programs that might draw images in the same terminal.
const FIRST_ID: u32 = 1000;

static NEXT_ID: AtomicU32 = AtomicU32::new(FIRST_ID);
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

/// How image data gets to the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Medium {
    /// Write each image to a file in this folder; the terminal reads and deletes it.
    TempFile(PathBuf),
    /// Send the pixels inline as base64. Works everywhere, including over SSH, but every byte
    /// goes through the terminal.
    Direct,
}

impl Medium {
    /// A fresh file name the terminal will accept as a temporary file: it must live in a
    /// temporary folder and have "tty-graphics-protocol" in its name.
    pub fn temp_file(&self) -> Option<PathBuf> {
        let Medium::TempFile(dir) = self else {
            return None;
        };
        let unique = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        Some(dir.join(format!(
            "tty-graphics-protocol-toshokan-{}-{unique}",
            std::process::id()
        )))
    }
}

/// Decides whether to draw images with this module, and how to send them.
///
/// Returns `None` when the terminal doesn't speak the kitty protocol, or inside tmux, where
/// placeholders need passthrough handling; `ratatui-image` covers those cases. Must run
/// before anything else reads terminal input, and with raw mode on.
pub fn detect(picker: &Picker) -> Option<Medium> {
    if picker.protocol_type() != ProtocolType::Kitty || picker.tmux_detected() {
        return None;
    }
    Some(match supports_temp_files() {
        Some(dir) => Medium::TempFile(dir),
        None => Medium::Direct,
    })
}

/// Asks the terminal to read a 1x1 image from a temporary file. A terminal on another machine
/// (SSH) or one without file support says no, and images are then sent inline.
fn supports_temp_files() -> Option<PathBuf> {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "tty-graphics-protocol-toshokan-{}-probe",
        std::process::id()
    ));
    fs::write(&path, [0u8; 3]).ok()?;
    // The device attributes request after the query is answered by every terminal, so the
    // reply arrives without waiting for a timeout even if the graphics query is ignored.
    let query = format!(
        "\x1b_Gi={PROBE_ID},a=q,t=t,f=24,s=1,v=1;{}\x1b\\\x1b[c",
        base64_simd::STANDARD.encode_to_string(path.as_os_str().as_encoded_bytes())
    );
    let mut stdout = io::stdout();
    let sent = stdout
        .write_all(query.as_bytes())
        .and_then(|()| stdout.flush());
    let reply = if sent.is_ok() {
        read_reply(Duration::from_secs(2))
    } else {
        Vec::new()
    };
    // The terminal deletes the file when it reads it; clean up in case it didn't.
    let _ = fs::remove_file(&path);
    let ok = format!("\x1b_Gi={PROBE_ID};OK");
    contains(&reply, ok.as_bytes()).then_some(dir)
}

/// Reads terminal replies from stdin until the device attributes reply (`ESC [ ? … c`) or the
/// timeout. Reads stdin directly rather than through a thread, so nothing is left behind
/// waiting to swallow a key press later.
fn read_reply(timeout: Duration) -> Vec<u8> {
    let stdin = rustix::stdio::stdin();
    let deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    let mut buf = [0u8; 512];
    while !has_device_attributes(&reply) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let timeout = Timespec {
            tv_sec: left.as_secs() as _,
            tv_nsec: left.subsec_nanos() as _,
        };
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => break,
            Ok(_) => {}
            // Poll again with the time that's left rather than risk a read that blocks.
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => break,
        }
        match rustix::io::read(stdin, &mut buf) {
            Ok(0) => break,
            Ok(read) => reply.extend_from_slice(&buf[..read]),
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
            Err(_) => break,
        }
    }
    reply
}

fn has_device_attributes(reply: &[u8]) -> bool {
    let mut rest = reply;
    while let Some(start) = find(rest, b"\x1b[?") {
        let after = &rest[start + 3..];
        let body = after
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b';')
            .count();
        match after.get(body) {
            Some(b'c') => return true,
            Some(_) => rest = &after[body..],
            None => return false,
        }
    }
    false
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

/// A new image id, cycling through `FIRST_ID..=MAX_ID`. Never 0, which the terminal would
/// read as "pick an id for me".
pub fn next_id() -> u32 {
    let advance = |id: u32| Some(if id >= MAX_ID { FIRST_ID } else { id + 1 });
    NEXT_ID
        .try_update(Ordering::Relaxed, Ordering::Relaxed, advance)
        .expect("advance always returns Some")
}

/// Sends an RGB image already written to `path` and creates its `cols` x `rows` placement.
/// Re-sending an id replaces the image; the terminal deletes the file once read.
pub fn transmit_file(out: &mut Vec<u8>, id: u32, size: (u32, u32), path: &Path, cell: (u16, u16)) {
    let (cols, rows) = cell;
    let path = base64_simd::STANDARD.encode_to_string(path.as_os_str().as_encoded_bytes());
    let _ = write!(
        out,
        "\x1b_Ga=T,U=1,q=2,i={id},p=1,c={cols},r={rows},f=24,t=t,s={},v={};{path}\x1b\\",
        size.0, size.1
    );
}

/// Sends RGB pixels inline, in base64 chunks, and creates the placement.
pub fn transmit_direct(out: &mut Vec<u8>, id: u32, size: (u32, u32), rgb: &[u8], cell: (u16, u16)) {
    let (cols, rows) = cell;
    let data = base64_simd::STANDARD.encode_to_string(rgb);
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(CHUNK).collect();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        if index == 0 {
            let _ = write!(
                out,
                "\x1b_Ga=T,U=1,q=2,i={id},p=1,c={cols},r={rows},f=24,t=d,s={},v={},m={more};",
                size.0, size.1
            );
        } else {
            let _ = write!(out, "\x1b_Gq=2,m={more};");
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// Changes the size in cells an image is fitted into, without sending it again.
pub fn place(out: &mut Vec<u8>, id: u32, cell: (u16, u16)) {
    let (cols, rows) = cell;
    let _ = write!(out, "\x1b_Ga=p,U=1,q=2,i={id},p=1,c={cols},r={rows}\x1b\\");
}

/// Frees an image and its placements in the terminal.
pub fn delete(out: &mut Vec<u8>, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,q=2,i={id}\x1b\\");
}

/// An image's pixels on their way to the terminal.
pub enum Payload {
    File(TempFile),
    Rgb(Vec<u8>),
}

impl Payload {
    /// Writes the pixels to `file` for the terminal to read; without a file, or when writing
    /// fails (e.g. /dev/shm is full), they're sent inline.
    pub fn new(file: Option<PathBuf>, rgb: Vec<u8>) -> Self {
        let Some(path) = file else {
            return Payload::Rgb(rgb);
        };
        let file = TempFile(Some(path));
        match fs::write(file.path(), &rgb) {
            Ok(()) => Payload::File(file),
            Err(_) => Payload::Rgb(rgb),
        }
    }

    /// Sends the image as `id` and creates its `placed` placement.
    pub fn transmit(self, out: &mut Vec<u8>, id: u32, size: (u32, u32), placed: (u16, u16)) {
        match self {
            Payload::File(file) => {
                transmit_file(out, id, size, file.path(), placed);
                file.hand_over();
            }
            Payload::Rgb(rgb) => transmit_direct(out, id, size, &rgb, placed),
        }
    }
}

/// An image file waiting for the terminal. It's removed when dropped (the reader moved on,
/// the app quit, the result never arrived) unless it was handed to the terminal, which
/// deletes it after reading.
pub struct TempFile(Option<PathBuf>);

impl TempFile {
    fn path(&self) -> &Path {
        self.0.as_deref().expect("only empty after hand_over")
    }

    fn hand_over(mut self) {
        self.0 = None;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// The terminal's cell size in pixels, which follows font size changes; it reports its size in
/// pixels and cells.
pub fn cell_size() -> Option<(u16, u16)> {
    // Tests set the cell size themselves, whatever terminal they happen to run in.
    if cfg!(test) {
        return None;
    }
    let size = crossterm::terminal::window_size().ok()?;
    (size.columns > 0 && size.rows > 0 && size.width > 0 && size.height > 0)
        .then(|| (size.width / size.columns, size.height / size.rows))
}

/// Draws part of an image placement: placeholder cells for the rows starting at `row_offset`.
pub struct Placeholders {
    pub id: u32,
    pub row_offset: u16,
}

impl Widget for Placeholders {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let color = Color::Rgb((self.id >> 16) as u8, (self.id >> 8) as u8, self.id as u8);
        let width = CellDiffOption::ForcedWidth(1.try_into().expect("non-zero"));
        let mut first = String::new();
        let plain = PLACEHOLDER.to_string();
        for y in 0..area.height {
            let row = usize::from(self.row_offset + y);
            let Some(&row_mark) = DIACRITICS.get(row) else {
                break;
            };
            // Every addressable cell says where it is, so a run that restarts after an
            // overlay (like the end-of-chapter popup) still shows the right part of the image.
            // Cells past the column table continue the run from their left neighbour.
            for x in 0..area.width {
                let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) else {
                    continue;
                };
                match DIACRITICS.get(usize::from(x)) {
                    Some(&column) => {
                        first.clear();
                        first.extend([PLACEHOLDER, row_mark, column]);
                        cell.set_symbol(&first);
                    }
                    None => {
                        cell.set_symbol(&plain);
                    }
                }
                cell.set_fg(color).set_diff_option(width);
            }
        }
    }
}

/// Combining characters that encode a placeholder cell's row and column, from the kitty
/// protocol's `rowcolumn-diacritics.txt`. Row `n` is `DIACRITICS[n]`.
const DIACRITICS: [char; 297] = [
    '\u{305}',
    '\u{30D}',
    '\u{30E}',
    '\u{310}',
    '\u{312}',
    '\u{33D}',
    '\u{33E}',
    '\u{33F}',
    '\u{346}',
    '\u{34A}',
    '\u{34B}',
    '\u{34C}',
    '\u{350}',
    '\u{351}',
    '\u{352}',
    '\u{357}',
    '\u{35B}',
    '\u{363}',
    '\u{364}',
    '\u{365}',
    '\u{366}',
    '\u{367}',
    '\u{368}',
    '\u{369}',
    '\u{36A}',
    '\u{36B}',
    '\u{36C}',
    '\u{36D}',
    '\u{36E}',
    '\u{36F}',
    '\u{483}',
    '\u{484}',
    '\u{485}',
    '\u{486}',
    '\u{487}',
    '\u{592}',
    '\u{593}',
    '\u{594}',
    '\u{595}',
    '\u{597}',
    '\u{598}',
    '\u{599}',
    '\u{59C}',
    '\u{59D}',
    '\u{59E}',
    '\u{59F}',
    '\u{5A0}',
    '\u{5A1}',
    '\u{5A8}',
    '\u{5A9}',
    '\u{5AB}',
    '\u{5AC}',
    '\u{5AF}',
    '\u{5C4}',
    '\u{610}',
    '\u{611}',
    '\u{612}',
    '\u{613}',
    '\u{614}',
    '\u{615}',
    '\u{616}',
    '\u{617}',
    '\u{657}',
    '\u{658}',
    '\u{659}',
    '\u{65A}',
    '\u{65B}',
    '\u{65D}',
    '\u{65E}',
    '\u{6D6}',
    '\u{6D7}',
    '\u{6D8}',
    '\u{6D9}',
    '\u{6DA}',
    '\u{6DB}',
    '\u{6DC}',
    '\u{6DF}',
    '\u{6E0}',
    '\u{6E1}',
    '\u{6E2}',
    '\u{6E4}',
    '\u{6E7}',
    '\u{6E8}',
    '\u{6EB}',
    '\u{6EC}',
    '\u{730}',
    '\u{732}',
    '\u{733}',
    '\u{735}',
    '\u{736}',
    '\u{73A}',
    '\u{73D}',
    '\u{73F}',
    '\u{740}',
    '\u{741}',
    '\u{743}',
    '\u{745}',
    '\u{747}',
    '\u{749}',
    '\u{74A}',
    '\u{7EB}',
    '\u{7EC}',
    '\u{7ED}',
    '\u{7EE}',
    '\u{7EF}',
    '\u{7F0}',
    '\u{7F1}',
    '\u{7F3}',
    '\u{816}',
    '\u{817}',
    '\u{818}',
    '\u{819}',
    '\u{81B}',
    '\u{81C}',
    '\u{81D}',
    '\u{81E}',
    '\u{81F}',
    '\u{820}',
    '\u{821}',
    '\u{822}',
    '\u{823}',
    '\u{825}',
    '\u{826}',
    '\u{827}',
    '\u{829}',
    '\u{82A}',
    '\u{82B}',
    '\u{82C}',
    '\u{82D}',
    '\u{951}',
    '\u{953}',
    '\u{954}',
    '\u{F82}',
    '\u{F83}',
    '\u{F86}',
    '\u{F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

/// Rows of an image placement that placeholders can address.
pub const MAX_ROWS: u16 = DIACRITICS.len() as u16;
/// Columns of an image placement that placeholders can address.
pub const MAX_COLUMNS: u16 = DIACRITICS.len() as u16;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_device_attributes_reply() {
        assert!(has_device_attributes(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22;52c"));
        assert!(!has_device_attributes(b"\x1b[?62;22"));
        assert!(!has_device_attributes(b"\x1b[?1u"));
        assert!(has_device_attributes(b"\x1b[?1u\x1b[?6c"));
    }

    #[test]
    fn ids_wrap_around_without_reaching_zero() {
        NEXT_ID.store(MAX_ID, Ordering::Relaxed);
        assert_eq!(next_id(), MAX_ID);
        assert_eq!(next_id(), FIRST_ID);
    }

    #[test]
    fn writes_small_commands() {
        let mut out = Vec::new();
        transmit_file(
            &mut out,
            1234,
            (848, 1216),
            Path::new("/dev/shm/x"),
            (60, 43),
        );
        place(&mut out, 1234, (30, 21));
        delete(&mut out, 1234);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "\x1b_Ga=T,U=1,q=2,i=1234,p=1,c=60,r=43,f=24,t=t,s=848,v=1216;L2Rldi9zaG0veA==\x1b\\\
             \x1b_Ga=p,U=1,q=2,i=1234,p=1,c=30,r=21\x1b\\\
             \x1b_Ga=d,d=I,q=2,i=1234\x1b\\"
        );
    }

    #[test]
    fn chunks_inline_data() {
        let mut out = Vec::new();
        transmit_direct(&mut out, 7, (100, 100), &vec![0u8; 100 * 100 * 3], (10, 5));
        let text = String::from_utf8(out).unwrap();
        let commands: Vec<&str> = text.split("\x1b\\").filter(|c| !c.is_empty()).collect();
        // 30000 bytes are 40000 base64 characters: ten chunks.
        assert_eq!(commands.len(), 10);
        assert!(
            commands[0].starts_with("\x1b_Ga=T,U=1,q=2,i=7,p=1,c=10,r=5,f=24,t=d,s=100,v=100,m=1;")
        );
        assert!(commands[1].starts_with("\x1b_Gq=2,m=1;"));
        assert!(commands[9].starts_with("\x1b_Gq=2,m=0;"));
        let payload: usize = commands
            .iter()
            .map(|c| c.split_once(';').unwrap().1.len())
            .sum();
        assert_eq!(payload, 40000);
    }

    #[test]
    fn placeholders_carry_the_id_and_rows() {
        let area = Rect::new(0, 0, 3, 2);
        let mut buf = Buffer::empty(area);
        Placeholders {
            id: 0x01_02_03,
            row_offset: 5,
        }
        .render(area, &mut buf);
        let first: Vec<char> = buf[(0, 0)].symbol().chars().collect();
        assert_eq!(first, [PLACEHOLDER, DIACRITICS[5], DIACRITICS[0]]);
        let second: Vec<char> = buf[(1, 0)].symbol().chars().collect();
        assert_eq!(second, [PLACEHOLDER, DIACRITICS[5], DIACRITICS[1]]);
        assert_eq!(buf[(0, 1)].symbol().chars().nth(1), Some(DIACRITICS[6]));
        assert_eq!(buf[(2, 1)].fg, Color::Rgb(1, 2, 3));
    }
}
