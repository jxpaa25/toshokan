//! A model of Ghostty 1.3.1's kitty graphics state and unicode placeholder rendering.
//!
//! Ported from Ghostty's `src/terminal/kitty/graphics_command.zig` (parser),
//! `graphics_exec.zig` (execute/transmit/display/delete), `graphics_storage.zig` (image and
//! placement storage, eviction), `graphics_image.zig` (loading, temp files, validation),
//! `graphics_unicode.zig` (PlacementIterator, IncompletePlacement, renderPlacement, grid) and
//! `renderer/image.zig` (kittyUpdate / prepKittyVirtualPlacement). Anything Ghostty would only
//! log (and, with q=2, never report to the application) is collected in `errors` so tests can
//! fail on it.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};

use ratatui::buffer::Buffer;
use ratatui::style::Color;

pub const PLACEHOLDER: char = '\u{10EEEE}';
const MAX_DIMENSION: u32 = 10_000;
const MAX_SIZE: usize = 400 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quiet {
    No,
    Ok,
    Failures,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Rgb,
    Rgba,
    Png,
}

impl Format {
    fn bpp(self) -> usize {
        match self {
            Format::Rgb => 3,
            Format::Rgba => 4,
            Format::Png => unreachable!("validated before"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    Direct,
    File,
    TempFile,
    SharedMemory,
}

#[derive(Debug, Clone, Copy)]
pub struct Transmission {
    format: Format,
    medium: Medium,
    width: u32,
    height: u32,
    size: u32,
    offset: u32,
    image_id: u32,
    image_number: u32,
    placement_id: u32,
    compressed: bool,
    more_chunks: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Display {
    image_id: u32,
    image_number: u32,
    placement_id: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    x_offset: u32,
    y_offset: u32,
    columns: u32,
    rows: u32,
    virtual_placement: bool,
    parent_id: u32,
    z: i32,
}

#[derive(Debug, Clone, Copy)]
pub enum Delete {
    All(bool),
    Id {
        delete: bool,
        image_id: u32,
        placement_id: u32,
    },
    Newest {
        delete: bool,
        image_number: u32,
        placement_id: u32,
    },
    /// Other delete kinds (cursor, cell, column, row, z, range, frames); none are used by the
    /// reader, so the model only records them.
    Other(u8),
}

#[derive(Debug, Clone)]
pub enum Control {
    Query(Transmission),
    Transmit(Transmission),
    TransmitAndDisplay(Transmission, Display),
    Display(Display),
    Delete(Delete),
    Animation,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub control: Control,
    pub quiet: Quiet,
    pub data: Vec<u8>,
}

impl Command {
    fn transmission(&self) -> Option<Transmission> {
        match &self.control {
            Control::Query(t) | Control::Transmit(t) => Some(*t),
            Control::TransmitAndDisplay(t, _) => Some(*t),
            _ => None,
        }
    }

    fn display(&self) -> Option<Display> {
        match &self.control {
            Control::Display(d) => Some(*d),
            Control::TransmitAndDisplay(_, d) => Some(*d),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// graphics_command.zig: Parser
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PState {
    ControlKey,
    ControlKeyIgnore,
    ControlValue,
    ControlValueIgnore,
    Data,
}

struct Parser {
    kv: HashMap<u8, u32>,
    temp: [u8; 11],
    temp_len: usize,
    current: u8,
    data: Vec<u8>,
    state: PState,
}

impl Parser {
    fn new() -> Self {
        Parser {
            kv: HashMap::new(),
            temp: [0; 11],
            temp_len: 0,
            current: 0,
            data: Vec::new(),
            state: PState::ControlKey,
        }
    }

    fn feed(&mut self, c: u8) -> Result<(), String> {
        match self.state {
            PState::ControlKey => match c {
                b'=' => {
                    if self.temp_len != 1 {
                        self.state = PState::ControlValueIgnore;
                        self.temp_len = 0;
                    } else {
                        self.current = self.temp[0];
                        self.temp_len = 0;
                        self.state = PState::ControlValue;
                    }
                }
                b';' => self.state = PState::Data,
                _ => self.accumulate(c, PState::ControlKeyIgnore),
            },
            PState::ControlKeyIgnore => {
                if c == b'=' {
                    self.state = PState::ControlValueIgnore;
                }
            }
            PState::ControlValue => match c {
                b',' => self.finish_value(PState::ControlKey)?,
                b';' => self.finish_value(PState::Data)?,
                _ => self.accumulate(c, PState::ControlValueIgnore),
            },
            PState::ControlValueIgnore => match c {
                b',' => self.state = PState::ControlKeyIgnore,
                b';' => self.state = PState::Data,
                _ => {}
            },
            PState::Data => self.data.push(c),
        }
        Ok(())
    }

    fn accumulate(&mut self, c: u8, overflow: PState) {
        let idx = self.temp_len;
        self.temp_len += 1;
        if self.temp_len > self.temp.len() {
            self.state = overflow;
            self.temp_len = 0;
            return;
        }
        self.temp[idx] = c;
    }

    fn finish_value(&mut self, next: PState) -> Result<(), String> {
        self.state = next;
        if self.temp_len == 1 {
            let c = self.temp[0];
            if !c.is_ascii_digit() {
                self.kv.insert(self.current, u32::from(c));
                self.temp_len = 0;
                return Ok(());
            }
        }
        let text = std::str::from_utf8(&self.temp[..self.temp_len]).map_err(|e| e.to_string())?;
        let value = match self.current {
            b'z' | b'H' | b'V' => text
                .parse::<i32>()
                .map(|v| v as u32)
                .map_err(|e| format!("InvalidFormat: {e} in {text:?}"))?,
            _ => text
                .parse::<u32>()
                .map_err(|e| format!("InvalidFormat: {e} in {text:?}"))?,
        };
        self.kv.insert(self.current, value);
        self.temp_len = 0;
        Ok(())
    }

    fn complete(mut self) -> Result<Command, String> {
        match self.state {
            PState::ControlKey | PState::ControlKeyIgnore => {
                return Err("InvalidFormat: ended in a key".into());
            }
            PState::ControlValue => self.finish_value(PState::Data)?,
            PState::ControlValueIgnore | PState::Data => {}
        }
        let action = match self.kv.get(&b'a') {
            None => b't',
            Some(&v) => u8::try_from(v).map_err(|_| "InvalidFormat: action".to_string())?,
        };
        let kv = &self.kv;
        let control = match action {
            b'q' => Control::Query(parse_transmission(kv)?),
            b't' => Control::Transmit(parse_transmission(kv)?),
            b'T' => Control::TransmitAndDisplay(parse_transmission(kv)?, parse_display(kv)?),
            b'p' => Control::Display(parse_display(kv)?),
            b'd' => Control::Delete(parse_delete(kv)?),
            b'f' | b'a' | b'c' => Control::Animation,
            _ => return Err(format!("InvalidFormat: action {}", action as char)),
        };
        let quiet = match kv.get(&b'q') {
            None => Quiet::No,
            Some(0) => Quiet::No,
            Some(1) => Quiet::Ok,
            Some(2) => Quiet::Failures,
            Some(_) => return Err("InvalidFormat: q".into()),
        };
        let data = if self.data.is_empty() {
            Vec::new()
        } else {
            base64_simd::STANDARD
                .decode_to_vec(&self.data)
                .map_err(|e| format!("InvalidData: base64 {e}"))?
        };
        Ok(Command {
            control,
            quiet,
            data,
        })
    }
}

fn parse_transmission(kv: &HashMap<u8, u32>) -> Result<Transmission, String> {
    let mut t = Transmission {
        format: Format::Rgba,
        medium: Medium::Direct,
        width: 0,
        height: 0,
        size: 0,
        offset: 0,
        image_id: 0,
        image_number: 0,
        placement_id: 0,
        compressed: false,
        more_chunks: false,
    };
    if let Some(&v) = kv.get(&b'f') {
        t.format = match v {
            24 => Format::Rgb,
            32 => Format::Rgba,
            100 => Format::Png,
            _ => return Err("InvalidFormat: f".into()),
        };
    }
    if let Some(&v) = kv.get(&b't') {
        t.medium = match u8::try_from(v).map_err(|_| "InvalidFormat: t")? {
            b'd' => Medium::Direct,
            b'f' => Medium::File,
            b't' => Medium::TempFile,
            b's' => Medium::SharedMemory,
            _ => return Err("InvalidFormat: t".into()),
        };
    }
    if let Some(&v) = kv.get(&b's') {
        t.width = v;
    }
    if let Some(&v) = kv.get(&b'v') {
        t.height = v;
    }
    if let Some(&v) = kv.get(&b'S') {
        t.size = v;
    }
    if let Some(&v) = kv.get(&b'O') {
        t.offset = v;
    }
    if let Some(&v) = kv.get(&b'i') {
        t.image_id = v;
    }
    if let Some(&v) = kv.get(&b'I') {
        t.image_number = v;
    }
    if let Some(&v) = kv.get(&b'p') {
        t.placement_id = v;
    }
    if let Some(&v) = kv.get(&b'o') {
        match u8::try_from(v).map_err(|_| "InvalidFormat: o")? {
            b'z' => t.compressed = true,
            _ => return Err("InvalidFormat: o".into()),
        }
    }
    // `m` only counts for the direct medium.
    if t.medium == Medium::Direct
        && let Some(&v) = kv.get(&b'm')
    {
        t.more_chunks = v > 0;
    }
    Ok(t)
}

fn parse_display(kv: &HashMap<u8, u32>) -> Result<Display, String> {
    let mut d = Display::default();
    let get = |k: u8| kv.get(&k).copied();
    if let Some(v) = get(b'i') {
        d.image_id = v;
    }
    if let Some(v) = get(b'I') {
        d.image_number = v;
    }
    if let Some(v) = get(b'p') {
        d.placement_id = v;
    }
    if let Some(v) = get(b'x') {
        d.x = v;
    }
    if let Some(v) = get(b'y') {
        d.y = v;
    }
    if let Some(v) = get(b'w') {
        d.width = v;
    }
    if let Some(v) = get(b'h') {
        d.height = v;
    }
    if let Some(v) = get(b'X') {
        d.x_offset = v;
    }
    if let Some(v) = get(b'Y') {
        d.y_offset = v;
    }
    if let Some(v) = get(b'c') {
        d.columns = v;
    }
    if let Some(v) = get(b'r') {
        d.rows = v;
    }
    if let Some(v) = get(b'C')
        && v > 1
    {
        return Err("InvalidFormat: C".into());
    }
    if let Some(v) = get(b'U') {
        d.virtual_placement = match v {
            0 => false,
            1 => true,
            _ => return Err("InvalidFormat: U".into()),
        };
    }
    if let Some(v) = get(b'z') {
        d.z = v as i32;
    }
    if let Some(v) = get(b'P') {
        d.parent_id = v;
    }
    Ok(d)
}

fn parse_delete(kv: &HashMap<u8, u32>) -> Result<Delete, String> {
    let what = match kv.get(&b'd') {
        None => b'a',
        Some(&v) => u8::try_from(v).map_err(|_| "InvalidFormat: d")?,
    };
    let get = |k: u8| kv.get(&k).copied().unwrap_or(0);
    Ok(match what {
        b'a' | b'A' => Delete::All(what == b'A'),
        b'i' | b'I' => Delete::Id {
            delete: what == b'I',
            image_id: get(b'i'),
            placement_id: get(b'p'),
        },
        b'n' | b'N' => Delete::Newest {
            delete: what == b'N',
            image_number: get(b'I'),
            placement_id: get(b'p'),
        },
        b'c' | b'C' | b'f' | b'F' | b'p' | b'P' | b'q' | b'Q' | b'r' | b'R' | b'x' | b'X'
        | b'y' | b'Y' | b'z' | b'Z' => Delete::Other(what),
        _ => return Err("InvalidFormat: d".into()),
    })
}

// ---------------------------------------------------------------------------------------------
// graphics_storage.zig / graphics_image.zig
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Image {
    pub id: u32,
    pub number: u32,
    pub width: u32,
    pub height: u32,
    pub format: Format,
    compressed: bool,
    pub data: Vec<u8>,
    pub transmit_time: u64,
    implicit_id: bool,
}

impl Image {
    /// The pixel at (x, y), as RGB.
    pub fn pixel(&self, x: f64, y: f64) -> [u8; 3] {
        let x = (x.floor().max(0.0) as u32).min(self.width - 1) as usize;
        let y = (y.floor().max(0.0) as u32).min(self.height - 1) as usize;
        let bpp = self.format.bpp();
        let i = (y * self.width as usize + x) * bpp;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlacementKey {
    pub image_id: u32,
    pub external: bool,
    pub id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// A placement at the cursor. The reader never wants one: it would draw the page at
    /// wherever the cursor happens to be.
    Pin {
        x: u16,
        y: u16,
    },
    Virtual,
}

#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub location: Location,
    pub columns: u32,
    pub rows: u32,
    pub z: i32,
}

struct Loading {
    image: Image,
    data: Vec<u8>,
    display: Option<Display>,
    quiet: Quiet,
}

struct Load {
    image_id: u32,
    more: bool,
    implicit: bool,
    display: Option<Display>,
}

#[derive(Debug, Clone)]
struct Response {
    id: u32,
    number: u32,
    placement_id: u32,
    message: String,
}

impl Default for Response {
    /// Like Ghostty's `Response{}`: no id, message "OK".
    fn default() -> Self {
        Response {
            id: 0,
            number: 0,
            placement_id: 0,
            message: "OK".into(),
        }
    }
}

impl Response {
    fn ok_for(id: u32, number: u32, placement_id: u32) -> Self {
        Response {
            id,
            number,
            placement_id,
            message: "OK".into(),
        }
    }
    fn err(message: &str) -> Self {
        Response {
            message: message.into(),
            ..Default::default()
        }
    }
    fn is_ok(&self) -> bool {
        self.message == "OK"
    }
    fn is_empty(&self) -> bool {
        self.id == 0 && self.number == 0
    }
}

/// The kitty graphics state of one Ghostty screen, and its renderer.
pub struct Ghostty {
    /// Cell size in pixels, as the renderer knows it.
    pub cell: (u32, u32),
    pub images: HashMap<u32, Image>,
    /// Insertion-ordered stand-in for Ghostty's placement hash map.
    pub placements: Vec<(PlacementKey, Placement)>,
    loading: Option<Loading>,
    next_image_id: u32,
    next_internal_placement_id: u32,
    pub total_bytes: usize,
    pub total_limit: usize,
    clock: u64,
    /// Everything Ghostty would warn about or answer with an error.
    pub errors: Vec<String>,
    /// Replies Ghostty would write back to the application's stdin.
    pub responses: Vec<String>,
    /// Bytes that weren't kitty graphics commands.
    pub stray: Vec<u8>,
    pub evicted: Vec<u32>,
    pub commands: usize,
    /// The cursor, for pin placements.
    cursor: (u16, u16),
}

impl Ghostty {
    pub fn new(cell: (u32, u32)) -> Self {
        Ghostty {
            cell,
            images: HashMap::new(),
            placements: Vec::new(),
            loading: None,
            next_image_id: 2_147_483_647,
            next_internal_placement_id: 0,
            total_bytes: 0,
            total_limit: 320 * 1000 * 1000,
            clock: 0,
            errors: Vec::new(),
            responses: Vec::new(),
            stray: Vec::new(),
            evicted: Vec::new(),
            commands: 0,
            cursor: (0, 0),
        }
    }

    pub fn loading_in_progress(&self) -> bool {
        self.loading.is_some()
    }

    /// Feeds bytes written to the terminal; only APC sequences are interpreted.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            let Some(start) = find(rest, b"\x1b_") else {
                self.stray.extend_from_slice(rest);
                break;
            };
            self.stray.extend_from_slice(&rest[..start]);
            let body = &rest[start + 2..];
            let Some(end) = find(body, b"\x1b\\") else {
                self.errors.push("unterminated APC sequence".into());
                break;
            };
            self.apc(&body[..end]);
            rest = &body[end + 2..];
        }
    }

    fn apc(&mut self, body: &[u8]) {
        let Some((&b'G', rest)) = body.split_first() else {
            self.errors.push("unknown APC command".into());
            return;
        };
        let mut parser = Parser::new();
        for &c in rest {
            if let Err(err) = parser.feed(c) {
                self.errors
                    .push(format!("kitty graphics protocol error: {err}"));
                return;
            }
        }
        match parser.complete() {
            Ok(command) => {
                self.commands += 1;
                self.execute(&command);
            }
            Err(err) => self
                .errors
                .push(format!("kitty graphics protocol error: {err}")),
        }
    }

    pub fn execute(&mut self, cmd: &Command) {
        let mut quiet = cmd.quiet;
        let response = match &cmd.control {
            Control::Query(_) => Some(self.query(cmd)),
            Control::Display(d) => Some(self.display(d)),
            Control::Delete(d) => {
                self.delete(*d);
                Some(Response::default())
            }
            Control::Transmit(_) | Control::TransmitAndDisplay(..) => {
                if let Some(loading) = &mut self.loading {
                    match cmd.quiet {
                        Quiet::No => quiet = loading.quiet,
                        q => loading.quiet = q,
                    }
                }
                Some(self.transmit(cmd))
            }
            Control::Animation => Some(Response::err("ERROR: unimplemented action")),
        };
        if let Some(resp) = response {
            if !resp.is_ok() {
                self.errors.push(format!(
                    "erroneous kitty graphics response: {}",
                    resp.message
                ));
            }
            let send = match quiet {
                Quiet::No => !resp.is_empty(),
                Quiet::Ok => !resp.is_ok() && !resp.is_empty(),
                Quiet::Failures => false,
            };
            if send {
                self.responses.push(format!(
                    "i={},I={},p={};{}",
                    resp.id, resp.number, resp.placement_id, resp.message
                ));
            }
        }
    }

    fn query(&mut self, cmd: &Command) -> Response {
        let t = cmd.transmission().expect("query");
        if t.image_id == 0 {
            return Response::err("EINVAL: image ID required");
        }
        let mut result = Response::ok_for(t.image_id, t.image_number, t.placement_id);
        // Loading has side effects (temporary files are deleted) but stores nothing.
        let mut errors = Vec::new();
        if let Err(err) = load_image(cmd, &mut errors) {
            result.message = err;
        }
        result
    }

    fn transmit(&mut self, cmd: &Command) -> Response {
        let t = cmd.transmission().expect("transmit");
        let mut result = Response::ok_for(t.image_id, t.image_number, t.placement_id);
        if t.image_id > 0 && t.image_number > 0 {
            return Response::err("EINVAL: image ID and number are mutually exclusive");
        }
        let load = match self.load_and_add(cmd) {
            Ok(load) => load,
            Err(err) => {
                result.message = err;
                return result;
            }
        };
        if let Some(mut d) = load.display {
            assert!(!load.more);
            d.image_id = load.image_id;
            result = self.display(&d);
        }
        if load.more {
            return Response::default();
        }
        if load.implicit {
            return Response::default();
        }
        result.id = load.image_id;
        result
    }

    fn load_and_add(&mut self, cmd: &Command) -> Result<Load, String> {
        let t = cmd.transmission().expect("transmit");
        let mut loading = match self.loading.take() {
            Some(mut loading) => {
                if loading.data.len() + cmd.data.len() > MAX_SIZE {
                    self.loading = Some(loading);
                    return Err("EINVAL: invalid data".into());
                }
                loading.data.extend_from_slice(&cmd.data);
                if t.more_chunks {
                    let id = loading.image.id;
                    self.loading = Some(loading);
                    return Ok(Load {
                        image_id: id,
                        more: true,
                        implicit: false,
                        display: None,
                    });
                }
                loading
            }
            None => {
                let (image, data) = load_image(cmd, &mut self.errors)?;
                Loading {
                    image,
                    data,
                    display: cmd.display(),
                    quiet: cmd.quiet,
                }
            }
        };
        if loading.image.id == 0 {
            loading.image.id = self.next_image_id;
            self.next_image_id = self.next_image_id.wrapping_add(1);
            if loading.image.number == 0 {
                loading.image.implicit_id = true;
            }
        }
        if t.more_chunks {
            let id = loading.image.id;
            self.loading = Some(loading);
            return Ok(Load {
                image_id: id,
                more: true,
                implicit: false,
                display: None,
            });
        }
        let image = self.complete(loading.image, loading.data)?;
        let load = Load {
            image_id: image.id,
            more: false,
            implicit: image.implicit_id,
            display: loading.display,
        };
        self.add_image(image)?;
        Ok(load)
    }

    fn complete(&mut self, mut image: Image, data: Vec<u8>) -> Result<Image, String> {
        if image.compressed {
            return Err("simulator: zlib compression not modelled".into());
        }
        if image.format == Format::Png {
            return Err("simulator: PNG not modelled".into());
        }
        if image.width == 0 || image.height == 0 {
            return Err("EINVAL: dimensions required".into());
        }
        if image.width > MAX_DIMENSION || image.height > MAX_DIMENSION {
            return Err("EINVAL: dimensions too large".into());
        }
        let expected = image.width as usize * image.height as usize * image.format.bpp();
        if data.len() != expected {
            self.errors.push(format!(
                "unexpected length image id={} expected_len={expected} actual_len={}",
                image.id,
                data.len()
            ));
            return Err("EINVAL: invalid data".into());
        }
        self.clock += 1;
        image.transmit_time = self.clock;
        image.data = data;
        Ok(image)
    }

    fn add_image(&mut self, image: Image) -> Result<(), String> {
        if image.data.len() > self.total_limit {
            return Err("ENOMEM: out of memory".into());
        }
        let total = self.total_bytes + image.data.len();
        if total > self.total_limit && !self.evict(total - self.total_limit) {
            return Err("ENOMEM: out of memory".into());
        }
        // Replacing an image keeps its placements.
        if let Some(old) = self.images.get(&image.id) {
            self.total_bytes -= old.data.len();
        }
        self.total_bytes += image.data.len();
        self.images.insert(image.id, image);
        Ok(())
    }

    fn evict(&mut self, req: usize) -> bool {
        let mut candidates: Vec<(bool, u64, u32)> = self
            .images
            .values()
            .map(|img| {
                let used = self.placements.iter().any(|(k, _)| k.image_id == img.id);
                (used, img.transmit_time, img.id)
            })
            .collect();
        candidates.sort();
        let mut evicted = 0;
        for (_, _, id) in candidates {
            self.placements.retain(|(k, _)| k.image_id != id);
            if let Some(img) = self.images.remove(&id) {
                evicted += img.data.len();
                self.total_bytes -= img.data.len();
                self.evicted.push(id);
                if evicted > req {
                    return true;
                }
            }
        }
        false
    }

    fn display(&mut self, d: &Display) -> Response {
        if d.image_id == 0 && d.image_number == 0 {
            return Response::err("EINVAL: image ID or number required");
        }
        let mut result = Response::ok_for(d.image_id, d.image_number, d.placement_id);
        let image_id = if d.image_id != 0 {
            self.images.get(&d.image_id).map(|i| i.id)
        } else {
            self.images
                .values()
                .filter(|i| i.number == d.image_number)
                .max_by_key(|i| i.transmit_time)
                .map(|i| i.id)
        };
        let Some(image_id) = image_id else {
            result.message = "ENOENT: image not found".into();
            return result;
        };
        result.id = image_id;
        let location = if d.virtual_placement {
            if d.parent_id > 0 {
                result.message = "EINVAL: virtual placement cannot refer to a parent".into();
                return result;
            }
            Location::Virtual
        } else {
            Location::Pin {
                x: self.cursor.0,
                y: self.cursor.1,
            }
        };
        let placement = Placement {
            location,
            columns: d.columns,
            rows: d.rows,
            z: d.z,
        };
        self.add_placement(image_id, d.placement_id, placement);
        result
    }

    fn add_placement(&mut self, image_id: u32, placement_id: u32, p: Placement) {
        assert!(self.images.contains_key(&image_id));
        let key = if placement_id == 0 {
            let id = self.next_internal_placement_id;
            self.next_internal_placement_id = self.next_internal_placement_id.wrapping_add(1);
            PlacementKey {
                image_id,
                external: false,
                id,
            }
        } else {
            PlacementKey {
                image_id,
                external: true,
                id: placement_id,
            }
        };
        match self.placements.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = p,
            None => self.placements.push((key, p)),
        }
    }

    fn delete(&mut self, d: Delete) {
        match d {
            Delete::All(delete_images) => {
                self.placements
                    .retain(|(_, p)| p.location == Location::Virtual);
                if delete_images {
                    let ids: Vec<u32> = self.images.keys().copied().collect();
                    for id in ids {
                        self.delete_if_unused(id);
                    }
                }
            }
            Delete::Id {
                delete,
                image_id,
                placement_id,
            } => self.delete_by_id(image_id, placement_id, delete),
            Delete::Newest {
                delete,
                image_number,
                placement_id,
            } => {
                if let Some(id) = self
                    .images
                    .values()
                    .filter(|i| i.number == image_number)
                    .max_by_key(|i| i.transmit_time)
                    .map(|i| i.id)
                {
                    self.delete_by_id(id, placement_id, delete);
                }
            }
            Delete::Other(what) => self
                .errors
                .push(format!("simulator: delete d={} not modelled", what as char)),
        }
    }

    fn delete_by_id(&mut self, image_id: u32, placement_id: u32, delete_unused: bool) {
        if placement_id == 0 {
            self.placements.retain(|(k, _)| k.image_id != image_id);
        } else {
            self.placements.retain(|(k, _)| {
                *k != PlacementKey {
                    image_id,
                    external: true,
                    id: placement_id,
                }
            });
        }
        if delete_unused {
            self.delete_if_unused(image_id);
        }
    }

    fn delete_if_unused(&mut self, image_id: u32) {
        if self.placements.iter().any(|(k, _)| k.image_id == image_id) {
            return;
        }
        if let Some(img) = self.images.remove(&image_id) {
            self.total_bytes -= img.data.len();
        }
    }
}

/// `LoadingImage.init` + `readFile`: the image's metadata and its (still base64-decoded) data.
fn load_image(cmd: &Command, errors: &mut Vec<String>) -> Result<(Image, Vec<u8>), String> {
    let t = cmd.transmission().expect("transmission");
    let image = Image {
        id: t.image_id,
        number: t.image_number,
        width: t.width,
        height: t.height,
        format: t.format,
        compressed: t.compressed,
        data: Vec::new(),
        transmit_time: 0,
        implicit_id: false,
    };
    if t.medium == Medium::Direct {
        return Ok((image, cmd.data.clone()));
    }
    if cmd.data.contains(&0) {
        return Err("EINVAL: invalid data".into());
    }
    let raw = String::from_utf8(cmd.data.clone()).map_err(|_| "EINVAL: invalid data")?;
    let path = match t.medium {
        Medium::File | Medium::TempFile => match fs::canonicalize(&raw) {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(err) => {
                errors.push(format!("failed to get absolute path: {err} ({raw})"));
                return Err("EINVAL: invalid data".into());
            }
        },
        Medium::SharedMemory => return Err("simulator: shared memory not modelled".into()),
        Medium::Direct => unreachable!(),
    };
    if path.starts_with("/proc/")
        || path.starts_with("/sys/")
        || (path.starts_with("/dev/") && !path.starts_with("/dev/shm/"))
    {
        return Err("EINVAL: invalid data".into());
    }
    let temporary = t.medium == Medium::TempFile;
    if temporary {
        if !path_in_temp_dir(&path) {
            return Err("EINVAL: temporary file not in temp dir".into());
        }
        if !path.contains("tty-graphics-protocol") {
            return Err("EINVAL: temporary file not named correctly".into());
        }
    }
    let result = read_file(&path, t);
    if temporary && let Err(err) = fs::remove_file(&path) {
        errors.push(format!("failed to delete temporary file: {err}"));
    }
    Ok((image, result?))
}

fn read_file(path: &str, t: Transmission) -> Result<Vec<u8>, String> {
    let mut file = fs::File::open(path).map_err(|_| "EINVAL: invalid data".to_string())?;
    let meta = file
        .metadata()
        .map_err(|_| "EINVAL: invalid data".to_string())?;
    if !meta.is_file() {
        return Err("EINVAL: invalid data".into());
    }
    if t.offset > 0 {
        file.seek(SeekFrom::Start(u64::from(t.offset)))
            .map_err(|_| "EINVAL: invalid data".to_string())?;
    }
    let limit = if t.size > 0 {
        (t.size as usize).min(MAX_SIZE)
    } else {
        MAX_SIZE
    };
    let mut data = Vec::new();
    file.take(limit as u64)
        .read_to_end(&mut data)
        .map_err(|_| "EINVAL: invalid data".to_string())?;
    Ok(data)
}

fn path_in_temp_dir(path: &str) -> bool {
    if path.starts_with("/tmp") || path.starts_with("/dev/shm") {
        return true;
    }
    let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    if path.starts_with(&dir) {
        return true;
    }
    fs::canonicalize(&dir).is_ok_and(|real| path.starts_with(&*real.to_string_lossy()))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------------------------
// graphics_unicode.zig + renderer/image.zig
// ---------------------------------------------------------------------------------------------

include!("diacritics.rs");

fn diacritic_index(cp: u32) -> Option<u32> {
    GHOSTTY_DIACRITICS.binary_search(&cp).ok().map(|i| i as u32)
}

fn color_to_id(color: Color) -> u32 {
    match color {
        Color::Reset => 0,
        Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
        Color::Indexed(i) => u32::from(i),
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
    }
}

#[derive(Debug, Clone, Copy)]
struct Incomplete {
    x: u16,
    y: u16,
    image_id_low: u32,
    image_id_high: Option<u8>,
    placement_id: Option<u32>,
    row: Option<u32>,
    col: Option<u32>,
    width: u32,
}

impl Incomplete {
    fn init(x: u16, y: u16, fg: Color, underline: Color, cps: &[u32]) -> Self {
        let pid = color_to_id(underline);
        let mut result = Incomplete {
            x,
            y,
            image_id_low: color_to_id(fg),
            image_id_high: None,
            placement_id: (pid != 0).then_some(pid),
            row: None,
            col: None,
            width: 1,
        };
        if let Some(&cp) = cps.first() {
            result.row = diacritic_index(cp);
            if let Some(&cp) = cps.get(1) {
                result.col = diacritic_index(cp);
                if let Some(&cp) = cps.get(2) {
                    result.image_id_high = diacritic_index(cp).and_then(|v| u8::try_from(v).ok());
                }
            }
        }
        result
    }

    fn can_append(&self, other: &Incomplete) -> bool {
        self.image_id_low == other.image_id_low
            && self.placement_id == other.placement_id
            && (other.row.is_none() || other.row == self.row)
            && (other.col.is_none() || other.col == Some(self.col.unwrap() + self.width))
            && (other.image_id_high.is_none() || other.image_id_high == self.image_id_high)
    }

    fn complete(&self) -> VirtualPlacement {
        VirtualPlacement {
            x: self.x,
            y: self.y,
            image_id: self.image_id_low | (u32::from(self.image_id_high.unwrap_or(0)) << 24),
            placement_id: self.placement_id.unwrap_or(0),
            col: self.col.unwrap_or(0),
            row: self.row.unwrap_or(0),
            width: self.width,
            height: 1,
        }
    }
}

/// One run of placeholder cells, as `PlacementIterator` yields it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualPlacement {
    pub x: u16,
    pub y: u16,
    pub image_id: u32,
    pub placement_id: u32,
    pub col: u32,
    pub row: u32,
    pub width: u32,
    pub height: u32,
}

/// `RenderPlacement`: where on screen part of an image is drawn.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderPlacement {
    pub offset_x: u32,
    pub offset_y: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub dest_width: u32,
    pub dest_height: u32,
}

/// What one screen cell shows of an image.
#[derive(Debug, Clone, Copy)]
pub struct CellImage {
    pub image_id: u32,
    /// Source rectangle in image pixels: x0, y0, x1, y1.
    pub src: [f64; 4],
    /// Fraction of the cell covered by the image.
    pub coverage: f64,
}

impl CellImage {
    pub fn centre(&self) -> (f64, f64) {
        (
            (self.src[0] + self.src[2]) / 2.0,
            (self.src[1] + self.src[3]) / 2.0,
        )
    }
}

pub struct Screen {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<Option<CellImage>>,
    pub runs: Vec<(VirtualPlacement, Option<RenderPlacement>)>,
    pub placeholder_cells: Vec<(u16, u16)>,
    pub errors: Vec<String>,
    /// Pixels drawn just outside a run's own cells because of Ghostty's rounding.
    pub spill_pixels: f64,
}

impl Screen {
    pub fn at(&self, x: u16, y: u16) -> Option<CellImage> {
        self.cells[usize::from(y) * usize::from(self.width) + usize::from(x)]
    }
}

impl Ghostty {
    /// Lists the placeholder runs on screen, like `PlacementIterator`.
    pub fn runs(buf: &Buffer) -> (Vec<VirtualPlacement>, Vec<(u16, u16)>, Vec<String>) {
        let area = buf.area;
        let mut runs = Vec::new();
        let mut cells = Vec::new();
        let mut errors = Vec::new();
        for y in 0..area.height {
            let mut run: Option<Incomplete> = None;
            for x in 0..area.width {
                let cell = &buf[(area.x + x, area.y + y)];
                let mut chars = cell.symbol().chars();
                if chars.next() != Some(PLACEHOLDER) {
                    if let Some(prev) = run.take() {
                        runs.push(prev.complete());
                    }
                    continue;
                }
                cells.push((x, y));
                // The terminal must advance exactly one column for it, like ratatui assumes.
                let forced = matches!(
                    cell.diff_option,
                    ratatui::buffer::CellDiffOption::ForcedWidth(w) if w.get() == 1
                );
                if !forced {
                    errors.push(format!(
                        "placeholder at ({x},{y}) without a forced width of 1"
                    ));
                }
                let cps: Vec<u32> = chars.map(u32::from).collect();
                let curr = Incomplete::init(x, y, cell.fg, cell.underline_color, &cps);
                match &mut run {
                    Some(prev) if prev.can_append(&curr) => prev.width += 1,
                    _ => {
                        if let Some(prev) = run.take() {
                            runs.push(prev.complete());
                        }
                        let mut start = curr;
                        start.row.get_or_insert(0);
                        start.col.get_or_insert(0);
                        run = Some(start);
                    }
                }
            }
            if let Some(prev) = run {
                runs.push(prev.complete());
            }
        }
        (runs, cells, errors)
    }

    /// What every cell of the screen shows, like the renderer's `kittyUpdate`.
    pub fn render(&self, buf: &Buffer) -> Screen {
        let area = buf.area;
        let (cw, ch) = self.cell;
        let (runs, placeholder_cells, mut errors) = Self::runs(buf);
        let mut screen = Screen {
            width: area.width,
            height: area.height,
            cells: vec![None; usize::from(area.width) * usize::from(area.height)],
            runs: Vec::new(),
            placeholder_cells,
            errors: Vec::new(),
            spill_pixels: 0.0,
        };
        let any_virtual = self
            .placements
            .iter()
            .any(|(_, p)| p.location == Location::Virtual);
        for (_, p) in &self.placements {
            if let Location::Pin { x, y } = p.location {
                errors.push(format!("a non-virtual placement is drawn at ({x},{y})"));
            }
        }
        if !any_virtual {
            // Ghostty doesn't even look for placeholders.
            for run in runs {
                errors.push(format!(
                    "placeholder run for image {} but no virtual placements exist",
                    run.image_id
                ));
                screen.runs.push((run, None));
            }
            screen.errors = errors;
            return screen;
        }
        for run in runs {
            let Some(image) = self.images.get(&run.image_id) else {
                errors.push(format!(
                    "missing image for virtual placement, ignoring image_id={} at ({},{})",
                    run.image_id, run.x, run.y
                ));
                screen.runs.push((run, None));
                continue;
            };
            let rp = match self.render_placement(&run, image, &mut errors) {
                Ok(rp) => rp,
                Err(err) => {
                    errors.push(format!(
                        "error rendering virtual placement err={err} image_id={}",
                        run.image_id
                    ));
                    screen.runs.push((run, None));
                    continue;
                }
            };
            screen.runs.push((run, Some(rp)));
            if rp.dest_width == 0 || rp.dest_height == 0 {
                continue;
            }
            // The image part is drawn at the run's top-left cell plus the offsets.
            let x0 = f64::from(u32::from(run.x) * cw + rp.offset_x);
            let y0 = f64::from(u32::from(run.y) * ch + rp.offset_y);
            let (dw, dh) = (f64::from(rp.dest_width), f64::from(rp.dest_height));
            let (x1, y1) = (x0 + dw, y0 + dh);
            let run_x0 = f64::from(u32::from(run.x) * cw);
            let run_x1 = f64::from((u32::from(run.x) + run.width) * cw);
            let run_y0 = f64::from(u32::from(run.y) * ch);
            // Ghostty rounds the offset and the size separately, so a letterboxed run can
            // reach a pixel into the next row; anything more would draw over other cells.
            if x0 < run_x0 - 1.5
                || x1 > run_x1 + 1.5
                || y0 < run_y0 - 1.5
                || y1 > run_y0 + f64::from(ch) + 1.5
            {
                errors.push(format!(
                    "run at ({},{}) of image {} draws outside its cells: {x0}..{x1} x {y0}..{y1}",
                    run.x, run.y, run.image_id
                ));
            }
            let sx = |px: f64| f64::from(rp.source_x) + (px - x0) * f64::from(rp.source_width) / dw;
            let sy =
                |py: f64| f64::from(rp.source_y) + (py - y0) * f64::from(rp.source_height) / dh;
            let first_col = (x0 / f64::from(cw)).floor().max(0.0) as u32;
            let last_col = ((x1 / f64::from(cw)).ceil() as u32).min(u32::from(area.width));
            let first_row = (y0 / f64::from(ch)).floor().max(0.0) as u32;
            let last_row = ((y1 / f64::from(ch)).ceil() as u32).min(u32::from(area.height));
            for cy in first_row..last_row {
                for cx in first_col..last_col {
                    let (cx0, cy0) = (f64::from(cx * cw), f64::from(cy * ch));
                    let ix0 = x0.max(cx0);
                    let ix1 = x1.min(cx0 + f64::from(cw));
                    let iy0 = y0.max(cy0);
                    let iy1 = y1.min(cy0 + f64::from(ch));
                    if ix1 <= ix0 || iy1 <= iy0 {
                        continue;
                    }
                    let coverage = (ix1 - ix0) * (iy1 - iy0) / f64::from(cw * ch);
                    let own = cy == u32::from(run.y)
                        && cx >= u32::from(run.x)
                        && cx < u32::from(run.x) + run.width;
                    if !own {
                        // A sliver from rounding (checked above); the cell shows its own run.
                        screen.spill_pixels += (ix1 - ix0) * (iy1 - iy0);
                        continue;
                    }
                    let slot =
                        &mut screen.cells[cy as usize * usize::from(area.width) + cx as usize];
                    if slot.is_some() {
                        errors.push(format!("two runs draw into cell ({cx},{cy})"));
                    }
                    *slot = Some(CellImage {
                        image_id: image.id,
                        src: [sx(ix0), sy(iy0), sx(ix1), sy(iy1)],
                        coverage,
                    });
                }
            }
        }
        screen.errors = errors;
        screen
    }

    fn grid(
        &self,
        p: &VirtualPlacement,
        image: &Image,
        errors: &mut Vec<String>,
    ) -> Result<(u32, u32), String> {
        let placement = if p.placement_id > 0 {
            self.placements
                .iter()
                .find(|(k, _)| {
                    *k == PlacementKey {
                        image_id: p.image_id,
                        external: true,
                        id: p.placement_id,
                    }
                })
                .map(|(_, v)| *v)
        } else {
            let virtuals: Vec<Placement> = self
                .placements
                .iter()
                .filter(|(k, v)| k.image_id == p.image_id && v.location == Location::Virtual)
                .map(|(_, v)| *v)
                .collect();
            if virtuals.len() > 1 {
                errors.push(format!(
                    "image {} has {} virtual placements; Ghostty picks one by hash order",
                    p.image_id,
                    virtuals.len()
                ));
            }
            virtuals.first().copied()
        };
        let placement = placement.ok_or("PlacementMissingPlacement")?;
        let (cw, ch) = self.cell;
        let mut rows = placement.rows;
        let mut columns = placement.columns;
        if rows == 0 {
            rows = image.height.div_ceil(ch);
        }
        if columns == 0 {
            columns = image.width.div_ceil(cw);
        }
        if rows > u32::from(u16::MAX) || columns > u32::from(u16::MAX) {
            return Err("PlacementGridOutOfBounds".into());
        }
        Ok((rows, columns))
    }

    /// `Placement.renderPlacement`, line for line.
    pub fn render_placement(
        &self,
        p: &VirtualPlacement,
        image: &Image,
        errors: &mut Vec<String>,
    ) -> Result<RenderPlacement, String> {
        let (cell_width, cell_height) = self.cell;
        let (grid_rows, grid_columns) = self.grid(p, image, errors)?;
        let img_width = f64::from(image.width);
        let img_height = f64::from(image.height);

        let p_rows_px = f64::from(grid_rows * cell_height);
        let p_cols_px = f64::from(grid_columns * cell_width);
        let (x_offset, y_offset, x_scale, y_scale);
        if img_width * p_rows_px > img_height * p_cols_px {
            x_scale = p_cols_px / img_width.max(1.0);
            y_scale = x_scale;
            y_offset = (p_rows_px - img_height * y_scale) / 2.0;
            x_offset = 0.0;
        } else {
            y_scale = p_rows_px / img_height.max(1.0);
            x_scale = y_scale;
            x_offset = (p_cols_px - img_width * x_scale) / 2.0;
            y_offset = 0.0;
        }

        let scaled_x_offset = x_offset / x_scale;
        let scaled_y_offset = y_offset / y_scale;
        let scaled_width = img_width + scaled_x_offset * 2.0;
        let scaled_height = img_height + scaled_y_offset * 2.0;

        let vp_width = f64::from(p.width);
        let vp_height = f64::from(p.height);
        let vp_col = f64::from(p.col);
        let vp_row = f64::from(p.row);
        let grid_cols_f = f64::from(grid_columns);
        let grid_rows_f = f64::from(grid_rows);
        let mut src_width = scaled_width * (vp_width / grid_cols_f);
        let mut src_height = scaled_height * (vp_height / grid_rows_f);
        let mut src_x = scaled_width * (vp_col / grid_cols_f);
        let mut src_y = scaled_height * (vp_row / grid_rows_f);

        let mut dx = 0.0;
        let mut dy = 0.0;
        let mut width = f64::from(p.width * cell_width);
        let mut height = f64::from(p.height * cell_height);

        if src_y < scaled_y_offset {
            let offset = scaled_y_offset - src_y;
            src_height -= offset;
            dy = offset;
            height -= offset * y_scale;
            src_y = 0.0;
            if src_height > img_height {
                src_height = img_height;
                height = img_height * y_scale;
            }
        } else if src_y + src_height > scaled_height - scaled_y_offset {
            src_y -= scaled_y_offset;
            src_height = scaled_height - scaled_y_offset - src_y;
            src_height -= scaled_y_offset;
            height = src_height * y_scale;
        } else {
            src_y -= scaled_y_offset;
        }

        if src_x < scaled_x_offset {
            let offset = scaled_x_offset - src_x;
            src_width -= offset;
            dx = offset;
            width -= offset * x_scale;
            src_x = 0.0;
            if src_width > img_width {
                src_width = img_width;
                width = img_width * x_scale;
            }
        } else if src_x + src_width > scaled_width - scaled_x_offset {
            src_x -= scaled_x_offset;
            src_width = scaled_width - scaled_x_offset - src_x;
            src_width -= scaled_x_offset;
            width = src_width * x_scale;
        } else {
            src_x -= scaled_x_offset;
        }

        if src_width <= 0.0 || src_height <= 0.0 {
            return Ok(RenderPlacement::default());
        }

        let to_u32 = |v: f64, what: &str, errors: &mut Vec<String>| -> u32 {
            let r = v.round();
            if r < 0.0 {
                // @intFromFloat of a negative value into u32 is illegal behaviour in Zig.
                errors.push(format!("negative {what} {v} in renderPlacement"));
                return 0;
            }
            r as u32
        };
        Ok(RenderPlacement {
            offset_x: to_u32(dx * x_scale, "offset_x", errors),
            offset_y: to_u32(dy * y_scale, "offset_y", errors),
            source_x: to_u32(src_x, "source_x", errors),
            source_y: to_u32(src_y, "source_y", errors),
            source_width: to_u32(src_width, "source_width", errors),
            source_height: to_u32(src_height, "source_height", errors),
            dest_width: to_u32(width, "dest_width", errors),
            dest_height: to_u32(height, "dest_height", errors),
        })
    }
}

#[cfg(test)]
mod tests {
    //! The model against Ghostty's own unit tests.
    use super::*;
    use ratatui::widgets::Widget;

    fn store_with(w: u32, h: u32, cols: u32, rows: u32, cell: (u32, u32)) -> (Ghostty, Image) {
        let mut g = Ghostty::new(cell);
        let image = Image {
            id: 1,
            number: 0,
            width: w,
            height: h,
            format: Format::Rgb,
            compressed: false,
            data: vec![0; (w * h * 3) as usize],
            transmit_time: 0,
            implicit_id: false,
        };
        g.images.insert(1, image.clone());
        g.add_placement(
            1,
            0,
            Placement {
                location: Location::Virtual,
                columns: cols,
                rows,
                z: 0,
            },
        );
        (g, image)
    }

    fn vp(col: u32, row: u32, width: u32) -> VirtualPlacement {
        VirtualPlacement {
            x: 0,
            y: 0,
            image_id: 1,
            placement_id: 0,
            col,
            row,
            width,
            height: 1,
        }
    }

    fn rp(r: RenderPlacement) -> [u32; 8] {
        [
            r.offset_x,
            r.offset_y,
            r.source_x,
            r.source_y,
            r.source_width,
            r.source_height,
            r.dest_width,
            r.dest_height,
        ]
    }

    #[test]
    fn ghostty_dog_4x2() {
        let (g, image) = store_with(500, 306, 4, 2, (36, 80));
        let mut e = Vec::new();
        let r0 = g.render_placement(&vp(0, 0, 4), &image, &mut e).unwrap();
        assert_eq!(rp(r0), [0, 36, 0, 0, 500, 153, 144, 44]);
        let r1 = g.render_placement(&vp(0, 1, 4), &image, &mut e).unwrap();
        assert_eq!(rp(r1), [0, 0, 0, 153, 500, 153, 144, 44]);
        assert!(e.is_empty());
    }

    #[test]
    fn ghostty_diacritic_spot_checks() {
        assert_eq!(diacritic_index(0x483), Some(30));
        assert_eq!(diacritic_index(0x1d242), Some(294));
        assert!(GHOSTTY_DIACRITICS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn ghostty_runs() {
        use ratatui::layout::Rect;
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 2));
        let set = |buf: &mut Buffer, x: u16, y: u16, s: &str| {
            buf[(x, y)]
                .set_symbol(s)
                .set_fg(Color::Indexed(42))
                .set_diff_option(ratatui::buffer::CellDiffOption::ForcedWidth(
                    1.try_into().unwrap(),
                ));
        };
        // Continuation break: cols 0 then 2.
        set(&mut buf, 0, 0, "\u{10EEEE}\u{0305}\u{0305}");
        set(&mut buf, 1, 0, "\u{10EEEE}\u{0305}\u{030E}");
        // Bare cells continue; text ends a run.
        set(&mut buf, 3, 0, "\u{10EEEE}\u{030D}\u{030D}");
        set(&mut buf, 4, 0, "\u{10EEEE}");
        set(&mut buf, 5, 0, "\u{10EEEE}\u{030D}");
        // High byte.
        set(&mut buf, 0, 1, "\u{10EEEE}\u{0305}\u{0305}\u{030E}");
        let (runs, _, errors) = Ghostty::runs(&buf);
        assert!(errors.is_empty());
        let summary: Vec<_> = runs
            .iter()
            .map(|r| (r.x, r.y, r.image_id, r.row, r.col, r.width))
            .collect();
        assert_eq!(
            summary,
            [
                (0, 0, 42, 0, 0, 1),
                (1, 0, 42, 0, 2, 1),
                (3, 0, 42, 1, 1, 3),
                (0, 1, 33554474, 0, 0, 1),
            ]
        );
    }

    #[test]
    fn chunked_transmit_and_display_and_delete() {
        let mut g = Ghostty::new((10, 20));
        let mut out = Vec::new();
        crate::ui::kitty::transmit_direct(&mut out, 77, (100, 100), &vec![9u8; 30000], (10, 5));
        g.feed(&out);
        assert!(g.errors.is_empty(), "{:?}", g.errors);
        assert_eq!(g.images[&77].data.len(), 30000);
        assert_eq!(g.placements.len(), 1);
        assert_eq!((g.placements[0].1.columns, g.placements[0].1.rows), (10, 5));
        // Re-sending replaces the image and the placement.
        let mut out = Vec::new();
        crate::ui::kitty::transmit_direct(&mut out, 77, (50, 50), &vec![1u8; 7500], (4, 3));
        crate::ui::kitty::place(&mut out, 77, (6, 6));
        g.feed(&out);
        assert_eq!(g.images.len(), 1);
        assert_eq!(g.images[&77].width, 50);
        assert_eq!(g.placements.len(), 1);
        assert_eq!((g.placements[0].1.columns, g.placements[0].1.rows), (6, 6));
        let mut out = Vec::new();
        crate::ui::kitty::delete(&mut out, 77);
        g.feed(&out);
        assert!(g.images.is_empty() && g.placements.is_empty());
        assert!(g.errors.is_empty() && g.responses.is_empty() && g.stray.is_empty());
    }

    /// What happens to a page sent with id 0, which `kitty::next_id` returns once when its
    /// counter passes 2^24: Ghostty gives the image an id of its own, the placeholders (fg
    /// black = id 0) find nothing, and `a=d,d=I,i=0` can't free it.
    #[test]
    fn an_image_sent_with_id_zero_is_never_shown_or_freed() {
        use ratatui::layout::Rect;
        let mut g = Ghostty::new((10, 20));
        let mut out = Vec::new();
        crate::ui::kitty::transmit_direct(&mut out, 0, (10, 10), &[5u8; 300], (2, 1));
        g.feed(&out);
        assert!(g.errors.is_empty(), "{:?}", g.errors);
        assert_eq!(
            g.images.keys().copied().collect::<Vec<_>>(),
            [2_147_483_647]
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        crate::ui::kitty::Placeholders {
            id: 0,
            row_offset: 0,
        }
        .render(buf.area, &mut buf);
        let screen = g.render(&buf);
        assert!(screen.cells.iter().all(Option::is_none));
        assert!(
            !screen.errors.is_empty(),
            "missing image for virtual placement"
        );
        let mut out = Vec::new();
        crate::ui::kitty::delete(&mut out, 0);
        g.feed(&out);
        assert_eq!(g.images.len(), 1, "still held");
    }
}
