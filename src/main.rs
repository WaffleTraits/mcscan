//! mcscan - find (and fix) items in a Minecraft Java world.
//!
//! Step 1, `scan`:  read every inventory in the world in parallel and write
//!                  the matching item stacks to a results file.
//! Step 2, `list` / `show` / `apply`: review that file, pick findings from it
//!                  and delete or modify them in the world.

mod apply;
mod filter;
mod item;
mod nbt;
mod preset;
mod progress;
mod region;
mod results;
mod scan;
mod tui;
mod world;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::io::IsTerminal;
use std::path::PathBuf;

const AFTER_HELP: &str = "\
Typical session:
  mcscan scan /srv/minecraft/world --preset illegal        # 1. find
  mcscan list mcscan-results.jsonl                          # 2. review
  mcscan apply mcscan-results.jsonl --all --fix --dry-run   # 3. preview
  mcscan apply mcscan-results.jsonl --all --fix             # 4. do it (server stopped)

Or do all of it in one full-screen session:
  mcscan tui /srv/minecraft/world

Run `mcscan <command> --help` for details, `mcscan fields` for the filter
language and `mcscan rules` for what the illegal-item preset checks.";

#[derive(Parser)]
#[command(name = "mcscan", version, about = "Fast multi-threaded Minecraft world item scanner and editor", after_help = AFTER_HELP)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Step 1: scan a world and write matching items to a results file
    Scan(ScanArgs),
    /// Step 2: print the findings of a scan
    List(ListArgs),
    /// Show the full current NBT of selected findings
    Show(ShowArgs),
    /// Step 2: delete or modify selected findings in the world
    Apply(ApplyArgs),
    /// Full-screen interface: scan, browse, select and fix in one place
    #[command(long_about = "Full-screen interface: scan, browse, select and fix in one place.\n\n\
        Give it a world folder to scan it (with the illegal preset unless you pass --preset, --filter or \
        --all-items), or a results file written by `scan` to review that. Everything works from the \
        keyboard; the mouse can click rows, column headers and buttons and scroll lists. Press ? inside \
        for the keys.")]
    Tui(TuiArgs),
    /// Describe the rules of a preset
    Rules(RulesArgs),
    /// Describe the --filter expression language
    Fields,
}

#[derive(Args, Clone)]
struct ScanArgs {
    /// World folder (the one containing level.dat)
    world: PathBuf,
    /// Where to write the findings ("-" for standard output)
    #[arg(short, long, value_name = "FILE", default_value = "mcscan-results.jsonl")]
    output: String,
    /// Rule preset: "illegal" (built in) or the path of a preset JSON file
    #[arg(short, long, value_name = "NAME|FILE")]
    preset: Option<String>,
    /// Filter expression; repeat to require all of them (see `mcscan fields`)
    #[arg(short, long = "filter", value_name = "EXPR")]
    filters: Vec<String>,
    /// List every item stack in the world (no preset or filter needed)
    #[arg(long)]
    all_items: bool,
    /// Also enable the preset's strict rules (custom_data, lore, ...)
    #[arg(long)]
    strict: bool,
    /// Preset rules to switch on
    #[arg(long, value_name = "RULE", value_delimiter = ',')]
    enable: Vec<String>,
    /// Preset rules to switch off
    #[arg(long, value_name = "RULE", value_delimiter = ',')]
    disable: Vec<String>,
    /// What to scan: chunks (block entities), entities, players, level, data [default: all]
    #[arg(long, value_name = "KIND", value_delimiter = ',')]
    only: Vec<String>,
    /// Only scan these dimensions, e.g. --dim the_nether
    #[arg(long, value_name = "DIM", value_delimiter = ',')]
    dim: Vec<String>,
    /// Store each matching item's full SNBT in the results file
    #[arg(long)]
    snbt: bool,
    /// Worker threads [default: all cores]
    #[arg(short = 'j', long, value_name = "N")]
    threads: Option<usize>,
    /// No progress or summary output
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Table,
    Json,
    Csv,
}

#[derive(Args)]
struct ListArgs {
    /// Results file written by `scan` ("-" for standard input)
    results: String,
    #[command(flatten)]
    select: results::SelectArgs,
    /// Output format
    #[arg(long, value_enum, default_value = "table")]
    format: Format,
    /// Print only counts per rule, source and item
    #[arg(long)]
    summary: bool,
}

#[derive(Args)]
struct ShowArgs {
    /// Results file written by `scan`
    results: String,
    #[command(flatten)]
    select: results::SelectArgs,
    /// World folder, if it has moved since the scan
    #[arg(long, value_name = "DIR")]
    world: Option<PathBuf>,
    /// Print SNBT on one line instead of indented
    #[arg(long)]
    compact: bool,
}

#[derive(Args)]
struct ApplyArgs {
    /// Results file written by `scan`
    results: String,
    #[command(flatten)]
    select: results::SelectArgs,

    /// Repair exactly what the preset flagged (clamp, strip or delete; see `mcscan rules`)
    #[arg(long, help_heading = "Actions")]
    fix: bool,
    /// Delete the items
    #[arg(long, help_heading = "Actions")]
    delete: bool,
    /// Set the stack size
    #[arg(long, value_name = "N", help_heading = "Actions")]
    set_count: Option<i64>,
    /// Lower over-levelled enchantments to their vanilla maximum
    #[arg(long, help_heading = "Actions")]
    clamp_enchants: bool,
    /// Remove every enchantment
    #[arg(long, help_heading = "Actions")]
    strip_enchants: bool,
    /// Remove one enchantment (repeatable)
    #[arg(long, value_name = "NAME", help_heading = "Actions")]
    remove_enchant: Vec<String>,
    /// Set (or add) an enchantment level, e.g. sharpness=5 (repeatable)
    #[arg(long, value_name = "NAME=LEVEL", help_heading = "Actions")]
    set_enchant: Vec<String>,
    /// Remove an item component, e.g. unbreakable (repeatable)
    #[arg(long, value_name = "COMPONENT", help_heading = "Actions")]
    remove_component: Vec<String>,
    /// Set raw NBT inside the item, e.g. components.minecraft:damage=0 (repeatable)
    #[arg(long, value_name = "PATH=SNBT", help_heading = "Actions")]
    set: Vec<String>,
    /// Remove raw NBT inside the item, e.g. components.minecraft:lore (repeatable)
    #[arg(long, value_name = "PATH", help_heading = "Actions")]
    remove: Vec<String>,
    /// Replace the item id, e.g. stone
    #[arg(long, value_name = "ID", help_heading = "Actions")]
    set_id: Option<String>,

    /// Show what would happen without writing anything
    #[arg(long)]
    dry_run: bool,
    /// Ask about each finding individually
    #[arg(short, long)]
    interactive: bool,
    /// Do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
    /// Do not copy modified files to <world>/mcscan-backups/ first
    #[arg(long)]
    no_backup: bool,
    /// Modify the world even though a server/game has it open (dangerous)
    #[arg(long)]
    force: bool,
    /// World folder, if it has moved since the scan
    #[arg(long, value_name = "DIR")]
    world: Option<PathBuf>,
    /// Preset used by --fix / --clamp-enchants [default: the one the scan used]
    #[arg(short, long, value_name = "NAME|FILE")]
    preset: Option<String>,
    /// Worker threads [default: all cores]
    #[arg(short = 'j', long, value_name = "N")]
    threads: Option<usize>,
}

#[derive(Args)]
struct TuiArgs {
    #[command(flatten)]
    scan: ScanArgs,
    /// Start with mouse support off (press m inside to toggle; off lets the terminal select text)
    #[arg(long)]
    no_mouse: bool,
    /// Colours: `default` follows the terminal, `mc` looks like Midnight Commander (press t inside to switch)
    #[arg(long, value_enum, default_value_t, value_name = "NAME")]
    theme: tui::ThemeName,
    /// Do not copy modified files to <world>/mcscan-backups/ first
    #[arg(long)]
    no_backup: bool,
    /// Allow modifying a world that a server/game has open (dangerous)
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct RulesArgs {
    /// Preset to describe
    #[arg(short, long, value_name = "NAME|FILE", default_value = "illegal")]
    preset: String,
}

fn parse_rule(name: &str) -> Result<preset::Rule> {
    preset::Rule::parse(name).ok_or_else(|| {
        let known: Vec<&str> = preset::RULES.iter().map(|r| r.id()).collect();
        anyhow!("unknown rule {name:?}. Known rules: {}", known.join(", "))
    })
}

fn build_scan(a: &ScanArgs) -> Result<scan::ScanOptions> {
    let preset_spec = a.preset.clone();
    let preset = match &a.preset {
        Some(spec) => {
            let mut p = preset::Preset::load(spec)?;
            if a.strict {
                p.enable_all();
            }
            for r in &a.enable {
                p.set_enabled(parse_rule(r)?, true);
            }
            for r in &a.disable {
                p.set_enabled(parse_rule(r)?, false);
            }
            Some(p)
        }
        None => {
            if a.strict || !a.enable.is_empty() || !a.disable.is_empty() {
                bail!("--strict, --enable and --disable only make sense together with --preset");
            }
            None
        }
    };
    let filters = a.filters.iter().map(|f| filter::Filter::parse(f)).collect::<Result<Vec<_>>>()?;
    if preset.is_none() && filters.is_empty() && !a.all_items {
        bail!(
            "nothing to search for. Try one of:\n  \
             mcscan scan {0} --preset illegal\n  \
             mcscan scan {0} -f 'id == elytra'\n  \
             mcscan scan {0} --all-items",
            a.world.display()
        );
    }
    let categories = if a.only.is_empty() {
        world::Category::ALL.to_vec()
    } else {
        a.only
            .iter()
            .map(|s| {
                world::Category::parse(s)
                    .ok_or_else(|| anyhow!("unknown --only value {s:?} (chunks, entities, players, level, data)"))
            })
            .collect::<Result<Vec<_>>>()?
    };
    Ok(scan::ScanOptions {
        world: a.world.clone(),
        preset,
        preset_spec,
        filters,
        all_items: a.all_items,
        categories,
        dims: a.dim.clone(),
        output: a.output.clone(),
        snbt: a.snbt,
        quiet: a.quiet,
        threads: a.threads,
    })
}

fn scan_cmd(a: ScanArgs) -> Result<()> {
    scan::run(build_scan(&a)?)
}

fn tui_cmd(a: TuiArgs) -> Result<()> {
    let mut args = a.scan.clone();
    let asked_for_something = args.preset.is_some() || !args.filters.is_empty() || args.all_items;
    let loaded = if args.world.is_file() {
        // A results file: review it. A rescan repeats what its header records
        // unless the command line says otherwise.
        let path = args.world.to_string_lossy().into_owned();
        let (header, records) = results::load(&path)?;
        args.world = PathBuf::from(&header.world);
        args.output = path;
        if !asked_for_something {
            args.preset = header.preset.clone();
            args.filters = header.filters.clone();
            args.all_items = args.preset.is_none() && args.filters.is_empty();
        }
        Some((header, records))
    } else {
        if !args.world.is_dir() {
            bail!("{} is neither a world folder nor a results file", args.world.display());
        }
        if args.output == "-" {
            bail!("the TUI needs a real results file, not -o -");
        }
        if !asked_for_something {
            args.preset = Some("illegal".to_string());
        }
        build_scan(&args)?; // report bad options before the screen is taken over
        None
    };
    let threads = args.threads;
    let results_path = args.output.clone();
    let world = args.world.clone();
    tui::run(tui::Config {
        make_scan: std::sync::Arc::new(move || build_scan(&args)),
        results_path,
        world,
        loaded,
        mouse: !a.no_mouse,
        theme: a.theme,
        backup: !a.no_backup,
        force: a.force,
        threads,
    })
}

fn list(a: ListArgs) -> Result<()> {
    let (header, records) = results::load(&a.results)?;
    let picked = a.select.compile()?.pick(&records);
    let out = std::io::stdout();
    let mut w = std::io::BufWriter::new(out.lock());
    if a.summary {
        return results::print_summary(&mut w, &picked);
    }
    match a.format {
        Format::Table => {
            results::print_table(&mut w, &picked)?;
            if std::io::stdout().is_terminal() {
                use std::io::Write;
                writeln!(
                    w,
                    "\n{} of {} finding(s) from {}",
                    scan::human_count(picked.len() as u64),
                    scan::human_count(records.len() as u64),
                    header.world
                )?;
            }
            Ok(())
        }
        Format::Json => results::print_json(&mut w, &picked),
        Format::Csv => results::print_csv(&mut w, &picked),
    }
}

/// `apply` and `show` act on the world, so they demand an explicit selection.
fn require_selection(sel: &results::SelectArgs, command: &str) -> Result<()> {
    if sel.is_unrestricted() && !sel.all {
        bail!(
            "say which findings to {command}: -n 1,5-9, --rule RULE, --id GLOB, --holder, --source, --dim, --uuid, or --all"
        );
    }
    Ok(())
}

fn world_of(header: &results::Header, over: Option<PathBuf>) -> PathBuf {
    over.unwrap_or_else(|| PathBuf::from(&header.world))
}

fn show(a: ShowArgs) -> Result<()> {
    let (header, records) = results::load(&a.results)?;
    require_selection(&a.select, "show")?;
    let picked = a.select.compile()?.pick(&records);
    apply::show(&world_of(&header, a.world), &picked, !a.compact)
}

fn split_assignment<'a>(s: &'a str, what: &str) -> Result<(&'a str, &'a str)> {
    s.split_once('=').ok_or_else(|| anyhow!("{what} expects NAME=VALUE, got {s:?}"))
}

fn component_arg(s: &str) -> String {
    match s.strip_prefix('!') {
        Some(rest) => format!("!{}", item::norm_id(rest)),
        None => item::norm_id(s).into_owned(),
    }
}

fn check_count(n: i64) -> Result<()> {
    if !(1..=99).contains(&n) {
        bail!("the stack size must be between 1 and 99 (delete the item to remove it)");
    }
    Ok(())
}

/// "sharpness=5"
fn parse_set_enchant(s: &str) -> Result<(String, i64)> {
    let (name, level) = split_assignment(s, "set-enchant")?;
    let level: i64 = level.trim().parse().with_context(|| format!("set-enchant {s:?}: level is not a number"))?;
    if !(1..=255).contains(&level) {
        bail!("set-enchant {s:?}: the game only accepts levels 1-255");
    }
    Ok((item::norm_id(name.trim()).into_owned(), level))
}

/// "components.minecraft:damage=0"
fn parse_set(s: &str) -> Result<(Vec<nbt::Seg>, nbt::Tag)> {
    let (path, value) = split_assignment(s, "set")?;
    Ok((
        nbt::parse_path(path.trim())?,
        nbt::parse_snbt(value).with_context(|| format!("set {s:?}: value is not valid SNBT"))?,
    ))
}

fn apply_cmd(a: ApplyArgs) -> Result<()> {
    let (header, records) = results::load(&a.results)?;
    require_selection(&a.select, "apply to")?;
    let picked = a.select.compile()?.pick(&records);

    let mut ops = apply::Ops {
        delete: a.delete,
        fix: a.fix,
        set_count: a.set_count,
        clamp_enchants: a.clamp_enchants,
        strip_enchants: a.strip_enchants,
        set_id: a.set_id.as_deref().map(|s| item::norm_id(s).into_owned()),
        remove_enchant: a.remove_enchant.iter().map(|s| item::norm_id(s).into_owned()).collect(),
        remove_component: a.remove_component.iter().map(|s| component_arg(s)).collect(),
        ..Default::default()
    };
    if let Some(n) = ops.set_count {
        check_count(n)?;
    }
    for s in &a.set_enchant {
        ops.set_enchant.push(parse_set_enchant(s)?);
    }
    for s in &a.set {
        ops.set.push(parse_set(s)?);
    }
    for s in &a.remove {
        ops.remove.push(nbt::parse_path(s.trim())?);
    }
    if ops.delete {
        let mut rest = ops.clone();
        rest.delete = false;
        if !rest.is_empty() {
            bail!("--delete cannot be combined with other actions");
        }
    }

    let preset = if ops.needs_preset() {
        let spec = a.preset.clone().or_else(|| header.preset.clone()).unwrap_or_else(|| "illegal".to_string());
        let mut p = preset::Preset::load(&spec)?;
        // The scan decided which rules count; --fix only repairs what it recorded.
        p.enable_all();
        Some(p)
    } else {
        None
    };

    apply::run(
        apply::ApplyOptions {
            world: world_of(&header, a.world),
            ops,
            preset,
            dry_run: a.dry_run,
            backup: !a.no_backup,
            force: a.force,
            yes: a.yes,
            interactive: a.interactive,
            threads: a.threads,
        },
        picked,
    )
}

fn rules(a: RulesArgs) -> Result<()> {
    let p = preset::Preset::load(&a.preset)?;
    println!("Preset \"{}\" - {}", p.name, p.description);
    if !p.game_version.is_empty() {
        println!("Data tables generated from Minecraft {} (data version {})", p.game_version, p.data_version);
    }
    println!();
    for r in preset::RULES {
        let (what, fix) = r.describe();
        let state = if p.is_enabled(r) { "on " } else { "off" };
        println!("  {:<24} [{state}]  {what}", r.id());
        println!("  {:<24}        --fix: {fix}", "");
    }
    println!(
        "\nRules marked off are only checked with --strict or --enable RULE.\n\
         Switch any rule off for a scan with --disable RULE[,RULE...]."
    );
    Ok(())
}

fn main() {
    // Die quietly, like other command-line tools, when the reader of a pipe
    // goes away (`mcscan show ... | head`) instead of panicking in println!.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Scan(a) => scan_cmd(a),
        Command::List(a) => list(a),
        Command::Show(a) => show(a),
        Command::Apply(a) => apply_cmd(a),
        Command::Tui(a) => tui_cmd(a),
        Command::Rules(a) => rules(a),
        Command::Fields => {
            println!("{}", filter::HELP);
            Ok(())
        }
    };
    if let Err(e) = result {
        // A closed pipe (`mcscan list ... | head`) is not an error worth reporting.
        let pipe = std::io::ErrorKind::BrokenPipe;
        let broken_pipe = e.chain().any(|c| {
            c.downcast_ref::<std::io::Error>().map_or(false, |io| io.kind() == pipe)
                || c.downcast_ref::<serde_json::Error>().map_or(false, |j| j.io_error_kind() == Some(pipe))
        });
        if !broken_pipe {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}
