//! MicroType Express, the compressed font PowerPoint embeds inside an Embedded OpenType wrapper
//! (`TTEMBED_TTCOMPRESSED`). From the W3C Member Submission "MicroType Express (MTX) Font Format"
//! (2008): three LZCOMP blocks (the font tables in Compact Table Format, the glyphs' initial push
//! data and their remaining instructions), turned back into a TrueType font here.

/// Most bytes one expanded block, or the rebuilt font, may take.
const MAX_OUT: usize = 32 << 20;

/// The TrueType font in a MicroType Express blob, or none if the data is damaged.
pub(crate) fn decompress(b: &[u8]) -> Option<Vec<u8>> {
    let version = *b.first()?;
    let u24 = |i: usize| b.get(i..i + 3).map(|s| s.iter().fold(0usize, |v, &x| v << 8 | usize::from(x)));
    let (data2, data3) = (u24(4)?, u24(7)?);
    if data2 < 10 || data3 < data2 || b.len() < data3 {
        return None;
    }
    let rest = lzcomp(b.get(10..data2)?, version)?;
    let push = lzcomp(b.get(data2..data3)?, version)?;
    let code = lzcomp(b.get(data3..)?, version)?;
    ctf_to_sfnt(&rest, &push, &code)
}

// ---- LZCOMP (submission §3 and appendix C) ----

/// Bytes put in front of the history so copies can reach them from the first byte on.
const PRELOAD: usize = 2 * 32 * 96 + 4 * 256;

fn preload() -> Vec<u8> {
    let mut v = Vec::with_capacity(PRELOAD);
    for k in 0..32u8 {
        for j in 0..96u8 {
            v.extend([k, j]);
        }
    }
    for j in 0..=255u8 {
        v.extend([j; 4]);
    }
    v
}

/// One LZCOMP block, expanded.
fn lzcomp(input: &[u8], version: u8) -> Option<Vec<u8>> {
    let mut bits = Bits { data: input, pos: 0 };
    let run_length = version != 1 && bits.bit()?;
    let mut dist = Ahuff::new(8);
    let mut len = Ahuff::new(8);
    let out_len = bits.value(24)?;
    let mut ranges = 1;
    while (1usize << (3 * ranges)) < out_len {
        ranges += 1;
    }
    let dup2 = 256 + 8 * ranges;
    let mut sym = Ahuff::new(dup2 + 3);
    let mut buf = preload();
    let end = PRELOAD + out_len;
    buf.reserve(out_len);
    while buf.len() < end {
        let s = sym.read(&mut bits)?;
        if s < 256 {
            buf.push(s as u8);
        } else if s >= dup2 {
            // DUP2, DUP4, DUP6: one byte from 2, 4 or 6 back.
            let back = 2 * (s - dup2 + 1);
            buf.push(*buf.get(buf.len() - back)?);
        } else {
            let length = copy_length(s - 256, &mut len, &mut bits, out_len)?;
            let mut distance = 0;
            for _ in 0..(s - 256) / 8 + 1 {
                distance = distance << 3 | dist.read(&mut bits)?;
            }
            distance += 1;
            // Past 512 bytes back the shortest copy is 3 bytes, not 2.
            let length = if distance >= 512 { length + 1 } else { length };
            // The distance counts to the copy's last byte.
            let start = buf.len().checked_sub(distance + length - 1)?;
            if buf.len() + length > end {
                return None;
            }
            for j in start..start + length {
                buf.push(*buf.get(j)?);
            }
        }
    }
    let out = buf.split_off(PRELOAD);
    if run_length { run_length_decode(&out) } else { Some(out) }
}

/// A copy's length: 2 bits from the copy symbol, then 2 more per length symbol while the third
/// bit is set.
fn copy_length(mut b: usize, len: &mut Ahuff, bits: &mut Bits, most: usize) -> Option<usize> {
    let mut length = 0;
    loop {
        length = length << 2 | (b & 3);
        if b & 4 == 0 || length > most {
            break;
        }
        b = len.read(bits)?;
    }
    (length <= most).then_some(length + 2)
}

/// The run-length layer: an escape byte, then `escape 0` for the escape byte itself and
/// `escape n b` for `n` copies of `b`.
fn run_length_decode(data: &[u8]) -> Option<Vec<u8>> {
    let Some((&escape, mut rest)) = data.split_first() else { return Some(Vec::new()) };
    let mut out = Vec::with_capacity(data.len());
    while let Some((&v, r)) = rest.split_first() {
        rest = r;
        if v != escape {
            out.push(v);
            continue;
        }
        let (&n, r) = rest.split_first()?;
        rest = r;
        if n == 0 {
            out.push(escape);
            continue;
        }
        let (&b, r) = rest.split_first()?;
        rest = r;
        if out.len() + usize::from(n) > MAX_OUT {
            return None;
        }
        out.resize(out.len() + usize::from(n), b);
    }
    Some(out)
}

/// Bits, most significant first.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = byte >> (7 - self.pos % 8) & 1;
        self.pos += 1;
        Some(b == 1)
    }

    fn value(&mut self, n: u32) -> Option<usize> {
        (0..n).try_fold(0usize, |v, _| Some(v << 1 | usize::from(self.bit()?)))
    }
}

#[derive(Clone, Copy, Default)]
struct Node {
    up: usize,
    left: usize,
    right: usize,
    /// The symbol of a leaf.
    code: Option<usize>,
    weight: u32,
}

const ROOT: usize = 1;

/// An adaptive Huffman code over the symbols `0..range`: nodes `1..2 * range`, in decreasing
/// weight order (the sibling property), the root at 1.
struct Ahuff {
    tree: Vec<Node>,
    /// The node of each symbol.
    leaf: Vec<usize>,
}

impl Ahuff {
    fn new(range: usize) -> Ahuff {
        let mut tree = vec![Node::default(); 2 * range];
        for (i, n) in tree.iter_mut().enumerate() {
            n.up = i / 2;
            n.weight = 1;
            if i >= range {
                n.code = Some(i - range);
            } else {
                (n.left, n.right) = (2 * i, 2 * i + 1);
            }
        }
        for i in (ROOT..range).rev() {
            tree[i].weight = tree[2 * i].weight + tree[2 * i + 1].weight;
        }
        let mut h = Ahuff { tree, leaf: (range..2 * range).collect() };
        if range > 256 {
            // The symbol code starts out favouring the shortest copies and DUP2/DUP4.
            let bumps = [(256, 1), (257, 1), (range - 3, 12), (range - 2, 6)];
            for (s, n) in bumps {
                for _ in 0..n {
                    h.bump(h.leaf[s]);
                }
            }
        } else {
            for _ in 0..2 {
                for s in 0..range {
                    h.bump(h.leaf[s]);
                }
            }
        }
        h
    }

    fn read(&mut self, bits: &mut Bits) -> Option<usize> {
        let mut a = ROOT;
        loop {
            a = if bits.bit()? { self.tree[a].right } else { self.tree[a].left };
            if let Some(code) = self.tree[a].code {
                self.bump(a);
                return Some(code);
            }
        }
    }

    /// Count one more use of node `a`, swapping it (and then each ancestor) with the first node
    /// of its weight so the weights stay in order.
    fn bump(&mut self, mut a: usize) {
        while a != ROOT {
            let w = self.tree[a].weight;
            let mut b = a - 1;
            if self.tree[b].weight == w {
                // The root outweighs every other node, so this stops above it.
                while self.tree[b - 1].weight == w {
                    b -= 1;
                }
                if b > ROOT {
                    self.swap(a, b);
                    a = b;
                }
            }
            self.tree[a].weight = w + 1;
            a = self.tree[a].up;
        }
        self.tree[ROOT].weight += 1;
    }

    fn swap(&mut self, a: usize, b: usize) {
        let (up_a, up_b) = (self.tree[a].up, self.tree[b].up);
        self.tree.swap(a, b);
        self.tree[a].up = up_a;
        self.tree[b].up = up_b;
        for n in [a, b] {
            match self.tree[n].code {
                Some(code) => self.leaf[code] = n,
                None => {
                    let (l, r) = (self.tree[n].left, self.tree[n].right);
                    self.tree[l].up = n;
                    self.tree[r].up = n;
                }
            }
        }
    }
}

// ---- Compact Table Format (submission §4 to §7) ----

/// A reader over big-endian data.
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2)?.try_into().ok().map(u16::from_be_bytes)
    }

    fn i16(&mut self) -> Option<i16> {
        self.take(2)?.try_into().ok().map(i16::from_be_bytes)
    }

    /// 255USHORT (§6.1.1).
    fn u255(&mut self) -> Option<usize> {
        Some(match self.u8()? {
            253 => usize::from(self.u16()?),
            254 => 506 + usize::from(self.u8()?),
            255 => 253 + usize::from(self.u8()?),
            c => usize::from(c),
        })
    }

    /// 255SHORT (§6.1.2).
    fn s255(&mut self) -> Option<i16> {
        let mut code = self.u8()?;
        if code == 253 {
            return self.i16();
        }
        let negative = code == 250;
        if negative {
            code = self.u8()?;
        }
        let v = match code {
            255 => 250 + i16::from(self.u8()?),
            254 => 500 + i16::from(self.u8()?),
            c => i16::from(c),
        };
        Some(if negative { -v } else { v })
    }
}

/// Rebuild the TrueType font from the three expanded blocks: the font tables, with `glyf` and
/// `cvt ` in their compact forms and `loca` empty; the glyphs' push data; their instructions.
fn ctf_to_sfnt(ctf: &[u8], push: &[u8], code: &[u8]) -> Option<Vec<u8>> {
    let mut d = Cur { b: ctf, i: 0 };
    let version = d.take(4)?;
    let n = usize::from(d.u16()?);
    d.take(6)?;
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(n + 1);
    let mut total = 0usize;
    for _ in 0..n {
        let tag: [u8; 4] = d.take(4)?.try_into().ok()?;
        d.take(4)?;
        let offset = usize::try_from(u32::from_be_bytes(d.take(4)?.try_into().ok()?)).ok()?;
        let length = usize::try_from(u32::from_be_bytes(d.take(4)?.try_into().ok()?)).ok()?;
        // Entries may overlap: bound the copies, not just each one.
        total = total.checked_add(length).filter(|t| *t <= MAX_OUT)?;
        tables.push((tag, ctf.get(offset..offset.checked_add(length)?)?.to_vec()));
    }
    let table = |tag: &[u8; 4]| tables.iter().find(|t| &t.0 == tag).map(|t| t.1.as_slice());
    let num_glyphs = usize::from(u16::from_be_bytes(table(b"maxp")?.get(4..6)?.try_into().ok()?));
    let mut glyphs = match table(b"glyf") {
        Some(g) => Some(glyf(g, num_glyphs, push, code, Layout::Submission).or_else(|| glyf(g, num_glyphs, push, code, Layout::Other))?),
        None => None,
    };
    // shortcut: hdmx and VDMX (device-metric caches no renderer here reads) are dropped, not
    // decoded; decode them (§5.4, §5.5) if a consumer ever needs them.
    tables.retain(|(tag, _)| !matches!(tag, b"hdmx" | b"VDMX" | b"loca"));
    for (tag, data) in &mut tables {
        match &*tag {
            b"cvt " => *data = cvt(data)?,
            b"head" => {
                // The font's checksum is set at the end; `loca` offsets are long.
                data.get_mut(8..12)?.fill(0);
                if glyphs.is_some() {
                    data.get_mut(50..52)?.copy_from_slice(&1u16.to_be_bytes());
                }
            }
            b"glyf" => *data = std::mem::take(&mut glyphs.as_mut()?.glyf),
            b"maxp" => {
                // Rebuilt push instructions can be longer than the original ones.
                if let (Some(g), Some(m)) = (&glyphs, data.get_mut(26..28)) {
                    let most = u16::from_be_bytes([m[0], m[1]]).max(g.max_instructions);
                    m.copy_from_slice(&most.to_be_bytes());
                }
            }
            _ => {}
        }
    }
    if let Some(g) = glyphs {
        tables.push((*b"loca", g.loca));
    }
    sfnt(version, tables)
}

/// The rebuilt `glyf` and `loca` tables, and the longest glyph program in them.
struct Glyphs {
    glyf: Vec<u8>,
    loca: Vec<u8>,
    max_instructions: u16,
}

/// Turn the compact glyphs back into TrueType ones (§5.6 to §5.11, §6, §7), with long `loca`
/// offsets. The three streams must be used up exactly: that is how a wrong [`Layout`] shows.
fn glyf(data: &[u8], num_glyphs: usize, push: &[u8], code: &[u8], layout: Layout) -> Option<Glyphs> {
    let mut src = Glyph { g: Cur { b: data, i: 0 }, push: Cur { b: push, i: 0 }, code: Cur { b: code, i: 0 }, layout };
    let mut out = Glyphs { glyf: Vec::new(), loca: Vec::with_capacity(4 * (num_glyphs + 1)), max_instructions: 0 };
    for _ in 0..num_glyphs {
        out.loca.extend(u32::try_from(out.glyf.len()).ok()?.to_be_bytes());
        let ins = src.glyph(&mut out.glyf)?;
        out.max_instructions = out.max_instructions.max(ins);
        out.glyf.resize(out.glyf.len().next_multiple_of(4), 0);
        if out.glyf.len() > MAX_OUT {
            return None;
        }
    }
    out.loca.extend(u32::try_from(out.glyf.len()).ok()?.to_be_bytes());
    let padding = data.get(src.g.i..)?.iter().all(|b| *b == 0);
    (padding && src.push.i == push.len() && src.code.i == code.len()).then_some(out)
}

/// The three streams a glyph is read from, and how its points are coded.
struct Glyph<'a> {
    g: Cur<'a>,
    push: Cur<'a>,
    code: Cur<'a>,
    layout: Layout,
}

/// How point flags 20 to 119 are coded.
#[derive(Clone, Copy)]
enum Layout {
    /// The submission's table (§5.11), the one WOFF2 adopted: what almost every file uses.
    Submission,
    /// Another one, met in a Mac PowerPoint deck whose two faces then matched their original fonts
    /// glyph by glyph: the one-byte form with 4 bits each for x and y, x from 1, at
    /// `60 + 16 * k + 5 * signs`, y from `1 + 16 * k`. shortcut: nothing else was seen there, so
    /// other flags are refused (the face then draws in a stand-in); learn them if a file with them
    /// turns up.
    Other,
}

impl Glyph<'_> {
    /// Append one glyph to `out`; returns its instructions' length.
    fn glyph(&mut self, out: &mut Vec<u8>) -> Option<u16> {
        let contours = self.g.i16()?;
        if contours < 0 {
            return self.composite(out);
        }
        if contours == 0 {
            // An empty glyph is just its contour count.
            return Some(0);
        }
        let (contours, bbox) =
            if contours == 0x7FFF { (self.g.i16()?, Some([self.g.i16()?, self.g.i16()?, self.g.i16()?, self.g.i16()?])) } else { (contours, None) };
        let mut ends = Vec::with_capacity(usize::try_from(contours).ok()?);
        for i in 0..contours {
            let v = self.g.u255()?;
            ends.push(if i == 0 { v } else { ends.last()? + v });
        }
        let points = ends.last().map_or(0, |e| e + 1);
        let flags = self.g.take(points)?;
        let (mut x, mut y) = (0i32, 0i32);
        let mut xs = Vec::with_capacity(2 * points);
        let mut ys = Vec::with_capacity(2 * points);
        let mut bounds = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
        for &f in flags {
            let (dx, dy) = triplet(f & 0x7F, &mut self.g, self.layout)?;
            xs.extend(i16::try_from(dx).ok()?.to_be_bytes());
            ys.extend(i16::try_from(dy).ok()?.to_be_bytes());
            (x, y) = (x + dx, y + dy);
            // Coordinates are 16-bit: this also keeps the sums from overflowing.
            i16::try_from(x).ok()?;
            i16::try_from(y).ok()?;
            bounds = [bounds[0].min(x), bounds[1].min(y), bounds[2].max(x), bounds[3].max(y)];
        }
        let instructions = self.instructions()?;
        out.extend(contours.to_be_bytes());
        match bbox {
            Some(b) => b.iter().for_each(|v| out.extend(v.to_be_bytes())),
            None if points == 0 => out.extend([0; 8]),
            None => {
                for v in bounds {
                    out.extend(i16::try_from(v).ok()?.to_be_bytes());
                }
            }
        }
        for e in ends {
            out.extend(u16::try_from(e).ok()?.to_be_bytes());
        }
        let len = u16::try_from(instructions.len()).ok()?;
        out.extend(len.to_be_bytes());
        out.extend(instructions);
        // On-curve or not; each coordinate a 16-bit delta.
        out.extend(flags.iter().map(|f| u8::from(f & 0x80 == 0)));
        out.extend(xs);
        out.extend(ys);
        Some(len)
    }

    /// A composite glyph: its box and components are stored as in TrueType.
    fn composite(&mut self, out: &mut Vec<u8>) -> Option<u16> {
        let start = self.g.i;
        self.g.take(8)?;
        let mut flags;
        loop {
            flags = self.g.u16()?;
            let args = if flags & 0x0001 != 0 { 4 } else { 2 };
            let transform = if flags & 0x0008 != 0 {
                2
            } else if flags & 0x0040 != 0 {
                4
            } else if flags & 0x0080 != 0 {
                8
            } else {
                0
            };
            self.g.take(2 + args + transform)?;
            if flags & 0x0020 == 0 {
                break;
            }
        }
        out.extend((-1i16).to_be_bytes());
        out.extend(self.g.b.get(start..self.g.i)?);
        if flags & 0x0100 == 0 {
            return Some(0);
        }
        let instructions = self.instructions()?;
        let len = u16::try_from(instructions.len()).ok()?;
        out.extend(len.to_be_bytes());
        out.extend(instructions);
        Some(len)
    }

    /// The glyph program: push instructions for its push data, then the rest of its code.
    fn instructions(&mut self) -> Option<Vec<u8>> {
        let (push_count, code_size) = (self.g.u255()?, self.g.u255()?);
        let mut values: Vec<i16> = Vec::with_capacity(push_count.min(4096));
        while values.len() < push_count {
            // Hop codes repeat the value two back: A x1 A x2 A (and A x1 A x2 A x3 A).
            let hop = match self.push.b.get(self.push.i) {
                Some(251) => 1,
                Some(252) => 2,
                _ => 0,
            };
            if hop == 0 {
                values.push(self.push.s255()?);
                continue;
            }
            self.push.i += 1;
            let a = *values.get(values.len().checked_sub(2)?)?;
            for _ in 0..hop {
                values.extend([a, self.push.s255()?]);
            }
            values.push(a);
        }
        if values.len() != push_count {
            return None;
        }
        let mut out = push_instructions(&values);
        out.extend(self.code.take(code_size)?);
        Some(out)
    }
}

/// TrueType instructions pushing `values`: bytes with (N)PUSHB, others with (N)PUSHW.
fn push_instructions(values: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 * values.len() + 4);
    let mut rest = values;
    while let Some(&first) = rest.first() {
        let byte = |v: &i16| (0..=255).contains(v);
        let is_byte = byte(&first);
        let n = rest.iter().take(255).take_while(|v| byte(v) == is_byte).count();
        let (run, tail) = rest.split_at(n);
        match (n <= 8, is_byte) {
            (true, true) => out.push(0xB0 + n as u8 - 1),
            (true, false) => out.push(0xB8 + n as u8 - 1),
            (false, b) => out.extend([if b { 0x40 } else { 0x41 }, n as u8]),
        }
        for v in run {
            if is_byte {
                out.push(*v as u8);
            } else {
                out.extend(v.to_be_bytes());
            }
        }
        rest = tail;
    }
    out
}

/// One point's (dx, dy) from its flag byte (on-curve bit removed) and the bytes after it
/// (§5.11). The low two bits are the signs of x and y (set: positive).
fn triplet(f: u8, g: &mut Cur, layout: Layout) -> Option<(i32, i32)> {
    let sign = |bit: u8, v: i32| if bit & 1 == 1 { v } else { -v };
    let byte = |g: &mut Cur| g.u8().map(i32::from);
    Some(match (f, layout) {
        (0..10, _) => (0, sign(f, (i32::from(f & 14) << 7) + byte(g)?)),
        (10..20, _) => (sign(f, (i32::from((f - 10) & 14) << 7) + byte(g)?), 0),
        (20..84, Layout::Submission) => {
            let (k, b) = (i32::from(f - 20), byte(g)?);
            (sign(f, 1 + (k & 0x30) + (b >> 4)), sign(f >> 1, 1 + ((k & 0x0C) << 2) + (b & 15)))
        }
        (84..120, Layout::Submission) => {
            let (k, b0, b1) = (i32::from(f - 84), byte(g)?, byte(g)?);
            (sign(f, 1 + ((k / 12) << 8) + b0), sign(f >> 1, 1 + (((k % 12) >> 2) << 8) + b1))
        }
        (60..120, Layout::Other) if (f - 60) % 16 == 5 * (f & 3) => {
            let b = byte(g)?;
            (sign(f, 1 + (b >> 4)), sign(f >> 1, 1 + 16 * i32::from((f - 60) / 16) + (b & 15)))
        }
        (20..120, Layout::Other) => return None,
        (120..124, _) => {
            let (b0, b1, b2) = (byte(g)?, byte(g)?, byte(g)?);
            (sign(f, (b0 << 4) + (b1 >> 4)), sign(f >> 1, ((b1 & 15) << 8) + b2))
        }
        _ => {
            let (b0, b1, b2, b3) = (byte(g)?, byte(g)?, byte(g)?, byte(g)?);
            (sign(f, (b0 << 8) + b1), sign(f >> 1, (b2 << 8) + b3))
        }
    })
}

/// The `cvt ` table (§5.2): a count, then each value as a 1–3 byte difference from the one
/// before.
fn cvt(data: &[u8]) -> Option<Vec<u8>> {
    let mut c = Cur { b: data, i: 0 };
    let n = usize::from(c.u16()?);
    let mut out = Vec::with_capacity(2 * n);
    let mut v = 0i16;
    for _ in 0..n {
        let d = match c.u8()? {
            code @ 248..=255 => (i16::from(code) - 247) * 238 + i16::from(c.u8()?),
            code @ 239..=247 => -((i16::from(code) - 239) * 238 + i16::from(c.u8()?)),
            238 => c.i16()?,
            code => i16::from(code),
        };
        v = v.wrapping_add(d);
        out.extend(v.to_be_bytes());
    }
    Some(out)
}

/// An sfnt from its tables, in tag order, with their checksums and the font's.
fn sfnt(version: &[u8], mut tables: Vec<([u8; 4], Vec<u8>)>) -> Option<Vec<u8>> {
    tables.sort_by_key(|t| t.0);
    let n = u16::try_from(tables.len()).ok().filter(|n| *n < 0x1000)?;
    let pow = if n == 0 { 0 } else { 1u16 << (15 - n.leading_zeros()) };
    let mut out = Vec::from(version);
    for v in [n, pow * 16, pow.max(1).trailing_zeros() as u16, (n - pow) * 16] {
        out.extend(v.to_be_bytes());
    }
    let mut offset = 12 + 16 * tables.len();
    let mut head = None;
    for (tag, data) in &tables {
        out.extend(tag);
        out.extend(checksum(data).to_be_bytes());
        out.extend(u32::try_from(offset).ok()?.to_be_bytes());
        out.extend(u32::try_from(data.len()).ok()?.to_be_bytes());
        if tag == b"head" {
            head = Some(offset);
        }
        offset += data.len().next_multiple_of(4);
    }
    for (_, data) in &tables {
        out.extend(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    if out.len() > MAX_OUT {
        return None;
    }
    if let Some(h) = head {
        let adjust = 0xB1B0_AFBAu32.wrapping_sub(checksum(&out));
        out.get_mut(h + 8..h + 12)?.copy_from_slice(&adjust.to_be_bytes());
    }
    Some(out)
}

fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, c| {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        sum.wrapping_add(u32::from_be_bytes(w))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An LZCOMP block of literals only: no run-length layer, the length, each byte's code.
    fn literals(data: &[u8]) -> Vec<u8> {
        let mut bits = vec![false];
        bits.extend((0..24).rev().map(|i| data.len() >> i & 1 == 1));
        let mut ranges = 1;
        while (1usize << (3 * ranges)) < data.len() {
            ranges += 1;
        }
        let mut sym = Ahuff::new(256 + 8 * ranges + 3);
        for &b in data {
            let leaf = sym.leaf[usize::from(b)];
            let (mut path, mut a) = (Vec::new(), leaf);
            while a != ROOT {
                let up = sym.tree[a].up;
                path.push(sym.tree[up].right == a);
                a = up;
            }
            bits.extend(path.iter().rev());
            sym.bump(leaf);
        }
        bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |v, (i, &b)| v | u8::from(b) << (7 - i))).collect()
    }

    #[test]
    fn lzcomp_reads_adaptive_huffman_literals() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * i % 251) as u8).collect();
        let block = literals(&data);
        assert_eq!(lzcomp(&block, 3).as_deref(), Some(&data[..]));
        assert!(lzcomp(&block[..block.len() / 2], 3).is_none(), "cut short");
    }

    #[test]
    fn both_point_layouts_decode() {
        let point = |f, b: &[u8], layout| triplet(f, &mut Cur { b, i: 0 }, layout);
        assert_eq!(point(25, &[0x7C], Layout::Submission), Some((8, -29)));
        assert_eq!(point(84, &[5, 6], Layout::Submission), Some((-6, -7)));
        assert_eq!(point(81, &[0x7C], Layout::Other), Some((8, -29)));
        assert_eq!(point(25, &[0x7C], Layout::Other), None);
        assert_eq!(point(124, &[0, 150, 0, 1], Layout::Other), Some((-150, -1)));
    }

    #[test]
    fn the_run_length_layer_expands_runs_and_escaped_escapes() {
        assert_eq!(run_length_decode(&[9, 1, 9, 0, 9, 3, 7, 2]).as_deref(), Some(&[1, 9, 7, 7, 7, 2][..]));
        assert!(run_length_decode(&[9, 1, 9]).is_none());
    }

    #[test]
    fn hop_codes_repeat_the_value_two_back_and_pushes_are_rebuilt() {
        // 7 1 Hop3(2) → 7 1 7 2 7; then a word and a negative value; then the code (CALL).
        let push = [7, 1, 251, 2, 253, 0x12, 0x34, 250, 9];
        let mut g = Glyph { g: Cur { b: &[7, 1], i: 0 }, push: Cur { b: &push, i: 0 }, code: Cur { b: &[0x2B], i: 0 }, layout: Layout::Submission };
        assert_eq!(g.instructions(), Some(vec![0xB4, 7, 1, 7, 2, 7, 0xB9, 0x12, 0x34, 0xFF, 0xF7, 0x2B]));
        // A hop code with fewer than two values before it is damaged data.
        let mut g = Glyph { g: Cur { b: &[3, 0], i: 0 }, push: Cur { b: &[251, 1], i: 0 }, code: Cur { b: &[], i: 0 }, layout: Layout::Submission };
        assert_eq!(g.instructions(), None);
    }

    /// Random blocks behind a valid header end in none (or a font), never a panic or a hang.
    #[test]
    fn damaged_micro_type_express_is_refused() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for n in 0..300 {
            let len = 10 + (next() % 4000) as usize;
            let mut b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let (d2, d3) = (10 + (next() as usize % (len - 9)), len - (n % 7));
            let d3 = d3.max(d2);
            b[0] = 3;
            b[4..7].copy_from_slice(&(d2 as u32).to_be_bytes()[1..]);
            b[7..10].copy_from_slice(&(d3 as u32).to_be_bytes()[1..]);
            let _ = decompress(&b);
        }
        assert!(decompress(&[3, 0, 0]).is_none());
    }
}
