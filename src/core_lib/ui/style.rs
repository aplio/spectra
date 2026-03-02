use std::io::{self, Write};

use crossterm::{
    queue,
    style::{Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underlined: bool,
    pub slow_blink: bool,
    pub rapid_blink: bool,
    pub reverse: bool,
    pub hidden: bool,
    pub crossed_out: bool,
}

pub fn apply_style<W: Write>(writer: &mut W, style: CellStyle) -> io::Result<()> {
    reset_style(writer)?;

    if let Some(fg) = style.fg {
        queue!(writer, SetForegroundColor(fg))?;
    }
    if let Some(bg) = style.bg {
        queue!(writer, SetBackgroundColor(bg))?;
    }

    if style.bold {
        queue!(writer, SetAttribute(Attribute::Bold))?;
    }
    if style.dim {
        queue!(writer, SetAttribute(Attribute::Dim))?;
    }
    if style.italic {
        queue!(writer, SetAttribute(Attribute::Italic))?;
    }
    if style.underlined {
        queue!(writer, SetAttribute(Attribute::Underlined))?;
    }
    if style.slow_blink {
        queue!(writer, SetAttribute(Attribute::SlowBlink))?;
    }
    if style.rapid_blink {
        queue!(writer, SetAttribute(Attribute::RapidBlink))?;
    }
    if style.reverse {
        queue!(writer, SetAttribute(Attribute::Reverse))?;
    }
    if style.hidden {
        queue!(writer, SetAttribute(Attribute::Hidden))?;
    }
    if style.crossed_out {
        queue!(writer, SetAttribute(Attribute::CrossedOut))?;
    }

    Ok(())
}

pub fn reset_style<W: Write>(writer: &mut W) -> io::Result<()> {
    queue!(writer, SetAttribute(Attribute::Reset), ResetColor)
}
