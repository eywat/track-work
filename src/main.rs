use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Error, Result};
use console::Term;
use crossbeam_channel::{bounded, select, tick, Receiver};
use csv::{ReaderBuilder, StringRecord, Writer};
use structopt::StructOpt;
use time::{Date, Duration, OffsetDateTime};

static DEBUG: AtomicBool = AtomicBool::new(false);

#[derive(Debug, StructOpt)]
#[structopt(name = "Track Work", about = "A simple work tracker.")]
struct Opt {
    /// Prints some debugging information
    #[structopt(short, long)]
    debug: bool,
    /// The file where the working data is stored
    #[structopt(parse(from_os_str), short, long, env = "TRACK_WORK_FILE")]
    file: PathBuf,
    /// The objective for this workin session, can be set anytime
    #[structopt(short, long, default_value = "")]
    objective: String,
    #[structopt(subcommand)]
    cmd: Command,
}

#[derive(Debug, StructOpt)]
enum Command {
    /// Start tracking work now
    Now,
    /// Stop the currently tracked session
    Stop,
    /// Start or display current sessions runtime, stops the current session when SIGINT is received
    Live,
    /// Displays info about time worked so far. See: info -h
    Info {
        #[structopt(short, long)]
        /// Show info for each session, otherwise shows data for current date and total duration
        uncompressed: bool,
        #[structopt(subcommand)]
        info: Option<Info>,
    },
}

#[derive(Debug, StructOpt)]
enum Info {
    /// Show data from <delta> months ago
    Month {
        #[structopt(default_value = "0")]
        /// Show data from <delta> months ago
        delta: u8,
    },
    /// Show data for all tracked dates
    All,
}

#[derive(Debug)]
struct Tracker {
    start: OffsetDateTime,
    end: Option<OffsetDateTime>,
    objective: String,
}

impl Tracker {
    fn start(objective: String) -> Self {
        Tracker {
            start: OffsetDateTime::now_local(),
            end: None,
            objective,
        }
    }
}

impl std::fmt::Display for Tracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = match self.end {
            Some(end) => end - self.start,
            None => OffsetDateTime::now_local() - self.start,
        };
        let duration = format!(
            "{:02}:{:02},",
            duration.whole_hours(),
            duration.whole_minutes() % 60
        );
        let end_str = match self.end {
            Some(end) => end.format("%R,"),
            None => ",".into(),
        };
        write!(
            f,
            "{} {} {} {}",
            self.start.format("%F, %R,"),
            end_str,
            duration,
            self.objective
        )
    }
}

impl From<StringRecord> for Tracker {
    fn from(rec: StringRecord) -> Self {
        let start = rec
            .get(0)
            .map(|s| OffsetDateTime::parse(s, "%F %T %z"))
            .expect("Could not read entry 0 of csv!")
            .expect("Could not parse start!");
        let end = rec
            .get(1)
            .map(|s| OffsetDateTime::parse(s, "%F %T %z").ok())
            .unwrap_or(None);
        let objective = rec.get(2).unwrap_or("").into();
        Self {
            start,
            end,
            objective,
        }
    }
}

fn debug() -> bool {
    DEBUG.load(Ordering::SeqCst)
}

fn read(path: &PathBuf) -> Result<Vec<Tracker>> {
    if path.exists() {
        let file = fs::File::open(path)
            .with_context(|| format!("Storage file not found: {}", path.display()))?;
        let mut rdr = ReaderBuilder::new().has_headers(true).from_reader(file);
        let data = rdr
            .records()
            .inspect(|data| {
                if debug() {
                    println!("{:?}", data)
                } else {
                }
            })
            .filter_map(|d| d.ok())
            .map(Tracker::from)
            .collect();
        Ok(data)
    } else {
        Ok(Vec::new())
    }
}

fn write(path: &PathBuf, data: &[Tracker]) -> Result<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .open(path)?;
    let mut writer = Writer::from_writer(file);
    if debug() {
        println!("{:?}", data);
    }
    writer.write_record(&["Start", "End", "Objective"])?;
    for entry in data.iter() {
        writer.write_record(&[
            entry.start.format("%F %T %z"),
            entry
                .end
                .map(|e| e.format("%F %T %z"))
                .unwrap_or_else(|| "".into()),
            entry.objective.clone(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

fn start(path: &PathBuf, objective: String, show: bool) -> Result<()> {
    let mut data = read(path)?;
    if let Some(entry) = data.last() {
        if entry.end.is_none() {
            return Err(Error::msg(
                "Last entry has no end. Please first correct this error",
            ));
        }
    }
    data.push(Tracker::start(objective));
    write(path, &data)?;
    if show {
        info(path, &None, false)?;
    }
    Ok(())
}

fn stop(path: &PathBuf, objective: String, show: bool) -> Result<()> {
    let mut data = read(path)?;
    if let Some(entry) = data.last_mut() {
        match entry.end {
            Some(_) => {
                return Err(Error::msg(
                    "Last entry already finished. There was no work to track!",
                ))
            }
            None => {
                let end = OffsetDateTime::now_local();
                entry.end = Some(end);
            }
        }
        entry.objective = objective;
    }
    write(path, &data)?;
    if show {
        info(path, &None, false)?;
    }
    Ok(())
}
fn get_month_data(
    data: Box<dyn Iterator<Item = Tracker>>,
    delta: u8,
) -> Box<dyn Iterator<Item = Tracker>> {
    let current = OffsetDateTime::now_local();
    let mut overflow = delta / 12;
    let delta = delta % 12;
    let month = if let Some(month) = current.month().checked_sub(delta) {
        month
    } else {
        overflow += 1;
        12 - (delta - current.month())
    };
    let year = current.year() - overflow as i32;
    Box::new(data.filter(move |m| m.start.month() == month && m.start.year() == year))
}

fn compress(data: Box<dyn Iterator<Item = Tracker>>) -> Box<dyn Iterator<Item = (Date, Duration)>> {
    let mut map = HashMap::new();
    for entry in data {
        let end = entry.end.unwrap_or_else(OffsetDateTime::now_local);
        let duration = map
            .entry(entry.start.date())
            .or_insert_with(|| Duration::new(0, 0));
        *duration += end - entry.start;
    }
    Box::new(map.into_iter())
}

fn info(path: &PathBuf, info: &Option<Info>, uncompressed: bool) -> Result<()> {
    let data = Box::new(read(path)?.into_iter());
    let info = info.as_ref().unwrap_or(&Info::Month { delta: 0 });
    if uncompressed {
        let entries = match info {
            Info::Month { delta } => get_month_data(data, *delta),
            Info::All => data,
        };
        println!("Date, Start, End, Duration, Objective");
        let total = entries
            .inspect(|e| println!("{}", e))
            .map(|e| e.end.unwrap_or_else(OffsetDateTime::now_local) - e.start)
            .fold(Duration::new(0, 0), |acc, e| acc + e);
        println!(
            "Total: {:02}:{:02}",
            total.whole_hours(),
            total.whole_minutes() % 60
        );
    } else {
        let entries = match info {
            Info::Month { delta } => compress(get_month_data(data, *delta)),
            Info::All => compress(data),
        };
        println!("Date, Duration");
        let total = entries
            .inspect(|e| {
                println!(
                    "{}: {:02}:{:02}",
                    e.0.format("%F"),
                    e.1.whole_hours(),
                    e.1.whole_minutes() % 60
                )
            })
            .map(|e| e.1)
            .fold(Duration::new(0, 0), |acc, e| acc + e);
        println!(
            "Total: {:02}:{:02}",
            total.whole_hours(),
            total.whole_minutes() % 60
        );
    }
    Ok(())
}

fn ctrl_channel() -> Result<Receiver<()>, ctrlc::Error> {
    let (sender, receiver) = bounded(100);
    ctrlc::set_handler(move || {
        let _ = sender.send(());
    })?;
    Ok(receiver)
}

fn live(path: &PathBuf, objective: String) -> Result<()> {
    let data = read(path)?;
    let start_time = match data.last() {
        Some(entry) if entry.end.is_none() => {
            println!("Tracking work started at {}", entry.start.format("%F %R"));
            entry.start
        }
        Some(_) | None => {
            let start_time = OffsetDateTime::now_local();
            println!("Tracking work starting now {}", start_time.format("%F %R"));
            start(path, "".into(), false)?;
            start_time
        }
    };
    let ctrl_c_events = ctrl_channel()?;
    let ticks = tick(std::time::Duration::from_secs(1));
    let term = Term::stdout();
    term.write_line("")?;
    loop {
        select! {
            recv(ticks) -> _ => {
                term.move_cursor_up(1)?;
                term.clear_line()?;
                let duration = OffsetDateTime::now_local() - start_time;
                let output = format!("Duration: {:02}:{:02}:{:02}",
                    duration.whole_hours(),
                    duration.whole_minutes()%60,
                    duration.whole_seconds()%60);
                term.write_line(&output)?;
            },
            recv(ctrl_c_events) -> _ => {
                println!();
                println!("Tracking finished");
                stop(path, objective, true)?;
                break;
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let opts = Opt::from_args();
    DEBUG.store(opts.debug, Ordering::SeqCst);
    if debug() {
        println!("{:?}", opts);
    }
    match opts.cmd {
        Command::Now => start(&opts.file, opts.objective, true),
        Command::Stop => stop(&opts.file, opts.objective, true),
        Command::Live => live(&opts.file, opts.objective),
        Command::Info {
            uncompressed,
            info: info_level,
        } => info(&opts.file, &info_level, uncompressed),
    }
}
