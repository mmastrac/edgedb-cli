use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::io;

use colorful::{Color, Colorful};
use const_format::concatcp;
use is_terminal::IsTerminal;
use snafu::{AsErrorSource, ResultExt, Snafu};
use terminal_size::{terminal_size, Width};
use tokio_stream::{Stream, StreamExt};

use edgedb_errors::display::display_error;

use crate::branding::BRANDING_CLI_CMD;
use crate::repl::VectorLimit;

pub use crate::msg;

mod buffer;
mod color;
mod formatter;
mod json;
mod native;
mod stream;
pub mod style;
#[cfg(test)]
mod tests;

pub use crate::error_display::print_query_warning as warning;
pub use crate::error_display::print_query_warnings as warnings;

use buffer::{Delim, Exception, UnwrapExc, WrapErr};
pub use color::Highlight;
use formatter::ColorfulExt;
pub(in crate::print) use formatter::Formatter;
pub(in crate::print) use native::FormatExt;
use stream::Output;

#[derive(Snafu, Debug)]
#[snafu(context(suffix(false)))]
pub enum PrintError<S: AsErrorSource + Error, P: AsErrorSource + Error> {
    #[snafu(display("error fetching element"))]
    StreamErr { source: S },
    #[snafu(display("error printing element"))]
    PrintErr { source: P },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub colors: Option<bool>,
    pub indent: usize,
    pub expand_strings: bool,
    pub max_width: Option<usize>,
    pub implicit_properties: bool,
    pub max_items: Option<usize>,
    pub max_vector_length: VectorLimit,
    pub styler: style::Styler,
}

pub(in crate::print) struct Printer<T> {
    // config
    colors: bool,
    indent: usize,
    expand_strings: bool,
    max_width: usize,
    implicit_properties: bool,
    max_items: Option<usize>,
    max_vector_length: VectorLimit,
    trailing_comma: bool,

    // state
    buffer: String,
    stream: T,
    delim: Delim,
    flow: bool,
    committed: usize,
    committed_indent: usize,
    committed_column: usize,
    column: usize,
    cur_indent: usize,

    styler: style::Styler,
}

struct Stdout {}

impl Config {
    pub fn new() -> Config {
        Config {
            colors: None,
            indent: 2,
            expand_strings: true,
            max_width: None,
            implicit_properties: false,
            max_items: None,
            max_vector_length: VectorLimit::Unlimited,
            styler: style::Styler::dark_256(),
        }
    }
    #[allow(dead_code)]
    pub fn max_width(&mut self, value: usize) -> &mut Config {
        self.max_width = Some(value);
        self
    }
    pub fn max_items(&mut self, value: Option<usize>) -> &mut Config {
        self.max_items = value;
        self
    }
    pub fn max_vector_length(&mut self, value: VectorLimit) -> &mut Config {
        self.max_vector_length = value;
        self
    }
    pub fn colors(&mut self, value: bool) -> &mut Config {
        self.colors = Some(value);
        self
    }
    pub fn expand_strings(&mut self, value: bool) -> &mut Config {
        self.expand_strings = value;
        self
    }
    pub fn implicit_properties(&mut self, value: bool) -> &mut Config {
        self.implicit_properties = value;
        self
    }
}

pub fn completion<B: AsRef<[u8]>>(res: B) {
    if use_color() {
        eprintln!(
            "{}",
            format!("OK: {}", String::from_utf8_lossy(res.as_ref()))
                .dark_gray()
                .bold()
        );
    } else {
        eprintln!("OK: {}", String::from_utf8_lossy(res.as_ref()));
    }
}

async fn format_rows_buf<S, I, E, O>(
    prn: &mut Printer<O>,
    rows: &mut S,
    row_buf: &mut Vec<I>,
    end_of_stream: &mut bool,
) -> Result<(), Exception<PrintError<E, O::Error>>>
where
    S: Stream<Item = Result<I, E>> + Send + Unpin,
    I: FormatExt,
    E: fmt::Debug + Error + 'static,
    O: Output,
    O::Error: fmt::Debug + Error + 'static,
{
    let branch = prn
        .open_block(prn.styler.apply(style::Style::SetLiteral, "{"))
        .wrap_err(PrintErr)?;

    debug_assert!(branch);
    while let Some(v) = rows.next().await.transpose().wrap_err(StreamErr)? {
        row_buf.push(v);
        if let Some(limit) = prn.max_items {
            if row_buf.len() > limit {
                prn.ellipsis().wrap_err(PrintErr)?;
                // consume extra items if any
                while rows.next().await.transpose().wrap_err(StreamErr)?.is_some() {}
                break;
            }
        }
        let v = row_buf.last().unwrap();
        v.format(prn).wrap_err(PrintErr)?;
        prn.comma().wrap_err(PrintErr)?;
        // Buffer rows up to one visual line.
        // After line is reached we get Exception::DisableFlow
    }
    *end_of_stream = true;
    prn.close_block(&prn.styler.apply(style::Style::SetLiteral, "}"), true)
        .wrap_err(PrintErr)?;
    Ok(())
}

async fn format_rows<S, I, E, O>(
    prn: &mut Printer<O>,
    buffered_rows: Vec<I>,
    rows: &mut S,
) -> Result<(), Exception<PrintError<E, O::Error>>>
where
    S: Stream<Item = Result<I, E>> + Send + Unpin,
    I: FormatExt,
    E: fmt::Debug + Error + 'static,
    O: Output,
    O::Error: fmt::Debug + Error + 'static,
{
    prn.reopen_block().wrap_err(PrintErr)?;
    let mut counter: usize = 0;
    for v in buffered_rows {
        counter += 1;
        if let Some(limit) = prn.max_items {
            if counter > limit {
                prn.ellipsis().wrap_err(PrintErr)?;
                break;
            }
        }
        v.format(prn).wrap_err(PrintErr)?;
        prn.comma().wrap_err(PrintErr)?;
    }
    while let Some(v) = rows.next().await.transpose().wrap_err(StreamErr)? {
        counter += 1;
        if let Some(limit) = prn.max_items {
            if counter > limit {
                prn.ellipsis().wrap_err(PrintErr)?;
                // consume extra items if any
                while rows.next().await.transpose().wrap_err(StreamErr)?.is_some() {}
                break;
            }
        }
        v.format(prn).wrap_err(PrintErr)?;
        prn.comma().wrap_err(PrintErr)?;
    }
    prn.close_block(&prn.styler.apply(style::Style::SetLiteral, "}"), true)
        .wrap_err(PrintErr)?;
    Ok(())
}

pub async fn native_to_stdout<S, I, E>(
    rows: S,
    config: &Config,
) -> Result<(), PrintError<E, io::Error>>
where
    S: Stream<Item = Result<I, E>> + Send + Unpin,
    I: FormatExt,
    E: fmt::Debug + Error + 'static,
{
    let w = config
        .max_width
        .unwrap_or_else(|| terminal_size().map(|(Width(w), _h)| w.into()).unwrap_or(80));
    let colors = config.colors.unwrap_or_else(|| io::stdout().is_terminal());
    _native_format(rows, config, w, colors, Stdout {}).await
}

async fn _native_format<S, I, E, O>(
    mut rows: S,
    config: &Config,
    max_width: usize,
    colors: bool,
    output: O,
) -> Result<(), PrintError<E, O::Error>>
where
    S: Stream<Item = Result<I, E>> + Send + Unpin,
    I: FormatExt,
    E: fmt::Debug + Error + 'static,
    O: Output,
    O::Error: Error + 'static,
{
    let mut prn = Printer {
        colors,
        indent: config.indent,
        expand_strings: config.expand_strings,
        max_width,
        implicit_properties: config.implicit_properties,
        max_items: config.max_items,
        max_vector_length: config.max_vector_length,
        trailing_comma: true,

        buffer: String::with_capacity(8192),
        stream: output,
        delim: Delim::None,
        flow: false,
        committed: 0,
        committed_indent: 0,
        committed_column: 0,
        column: 0,
        cur_indent: 0,

        styler: config.styler.clone(),
    };
    let mut row_buf = Vec::new();
    let mut eos = false;
    match format_rows_buf(&mut prn, &mut rows, &mut row_buf, &mut eos).await {
        Ok(()) => {}
        Err(Exception::DisableFlow) => {
            if !eos {
                format_rows(&mut prn, row_buf, &mut rows)
                    .await
                    .unwrap_exc()?;
            }
        }
        Err(Exception::Error(e)) => return Err(e),
    };
    prn.end().unwrap_exc().context(PrintErr)?;
    Ok(())
}

fn format_rows_str<I: FormatExt>(
    prn: &mut Printer<&mut String>,
    items: &[I],
    open: &str,
    close: &str,
    reopen: bool,
) -> buffer::Result<Infallible> {
    if reopen {
        prn.reopen_block()?;
    } else {
        let cp = prn.open_block(open.clear())?;
        debug_assert!(cp);
    }
    for v in items {
        v.format(prn)?;
        prn.comma()?;
    }
    prn.close_block(&close.clear(), true)?;
    Ok(())
}

pub fn json_to_string<I: FormatExt>(items: &[I], config: &Config) -> Result<String, Infallible> {
    let mut out = String::new();
    let mut prn = Printer {
        colors: config.colors.unwrap_or(false),
        indent: config.indent,
        expand_strings: config.expand_strings,
        max_width: config.max_width.unwrap_or(80),
        implicit_properties: config.implicit_properties,
        max_items: config.max_items,
        max_vector_length: config.max_vector_length,
        trailing_comma: false,

        buffer: String::with_capacity(8192),
        stream: &mut out,
        delim: Delim::None,
        flow: false,
        committed: 0,
        committed_indent: 0,
        committed_column: 0,
        column: 0,
        cur_indent: 0,

        styler: config.styler.clone(),
    };
    match format_rows_str(&mut prn, items, "[", "]", false) {
        Ok(()) => {}
        Err(Exception::DisableFlow) => {
            format_rows_str(&mut prn, items, "[", "]", true).unwrap_exc()?;
        }
        Err(Exception::Error(e)) => return Err(e),
    };
    prn.end().unwrap_exc()?;
    Ok(out)
}

pub fn json_item_to_string<I: FormatExt>(item: &I, config: &Config) -> Result<String, Infallible> {
    let mut out = String::new();
    let mut prn = Printer {
        colors: config.colors.unwrap_or(false),
        indent: config.indent,
        expand_strings: config.expand_strings,
        max_width: config.max_width.unwrap_or(80),
        implicit_properties: config.implicit_properties,
        max_items: config.max_items,
        max_vector_length: config.max_vector_length,
        trailing_comma: false,

        buffer: String::with_capacity(8192),
        stream: &mut out,
        delim: Delim::None,
        flow: false,
        committed: 0,
        committed_indent: 0,
        committed_column: 0,
        column: 0,
        cur_indent: 0,

        styler: config.styler.clone(),
    };
    match item.format(&mut prn) {
        Ok(()) => {}
        Err(Exception::DisableFlow) => unreachable!(),
        Err(Exception::Error(e)) => return Err(e),
    }
    prn.end().unwrap_exc()?;
    Ok(out)
}

pub fn use_color() -> bool {
    concolor::get(concolor::Stream::Stdout).ansi_color()
}

pub fn prompt(line: impl fmt::Display) {
    if use_color() {
        println!("{}", line.to_string().bold().color(Color::Orange3),);
    } else {
        println!("{line}");
    }
}

pub fn err_marker() -> impl fmt::Display {
    concatcp!(BRANDING_CLI_CMD, " error:").err_marker()
}

pub fn error(line: impl fmt::Display) {
    let text = format!("{line:#}");
    if text.len() > 60 {
        msg!("{} {}", err_marker(), text);
    } else {
        // Emphasise only short lines. Long lines with bold look ugly.
        msg!("{} {}", err_marker(), text.emphasize());
    }
}

pub fn edgedb_error(err: &edgedb_errors::Error, verbose: bool) {
    // Note: not using `error()` as display_error has markup inside
    msg!("{} {}", err_marker(), display_error(err, verbose));
}

pub fn success(line: impl fmt::Display) {
    if use_color() {
        msg!("{}", line.to_string().bold().light_green());
    } else {
        msg!("{line}");
    }
}

pub fn success_msg(title: impl fmt::Display, msg: impl fmt::Display) {
    if use_color() {
        msg!(
            "{}: {}",
            title.to_string().bold().light_green(),
            msg.to_string().bold().white()
        );
    } else {
        msg!("{title}: {msg}");
    }
}

pub fn warn(line: impl fmt::Display) {
    if use_color() {
        msg!("{}", line.to_string().bold().yellow());
    } else {
        msg!("{line}");
    }
}
