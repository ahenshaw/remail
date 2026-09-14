//! remail from a command line.
//!
//! Read-only, and answered from the cache unless asked otherwise. The cache
//! holds mailboxes, envelopes and the bodies of messages that have been
//! opened or prefetched, which is enough for most questions and costs no
//! round trip — the same query language the interface filters with evaluates
//! against it offline. `--server` escalates to an IMAP `SEARCH`, which
//! reaches mail that was never cached and needs a working account.
//!
//!     remail-cli folders
//!     remail-cli list --mailbox INBOX --limit 20
//!     remail-cli search 'from:dupr since:2w'
//!     remail-cli search --server --scope all 'subject:pickleball'
//!     remail-cli show 41349
//!
//! Output is JSON, for whatever is reading it; `--text` is for people.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use remail::config::{AccountId, Config};
use remail::mail::{
    Command, Engine, Envelope, Event, MessageBody, SearchScope, Store, parse, query::Query,
};

/// How long a command that reaches the server is given before giving up.
/// A whole-account search visits every folder, so this is generous.
const SERVER_TIMEOUT: Duration = Duration::from_secs(120);

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(remail::log_filter())
        .with_writer(std::io::stderr)
        .init();

    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // The cause chain, as the interface reports it: "connecting to
            // imap.example.com: dns error" beats either half alone.
            eprintln!(
                "remail-cli: {}",
                e.chain().map(|c| c.to_string()).collect::<Vec<_>>().join(": ")
            );
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    if args.help {
        print!("{USAGE}");
        return Ok(());
    }

    let config = Config::load().context("reading the configuration")?;
    let account = pick_account(&config, args.account)?;
    let store = open_store()?;

    match args.verb.as_str() {
        "folders" => folders(&store, account, &args),
        "list" => list(&store, &config, account, &args),
        "search" => search(store, config, account, &args),
        "show" => show(store, config, account, &args),
        other => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

// -- commands -------------------------------------------------------------

fn folders(store: &Store, account: AccountId, args: &Args) -> Result<()> {
    let mailboxes = store.load_mailboxes(account)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&mailboxes)?);
        return Ok(());
    }
    for mailbox in mailboxes {
        println!("{:<34} {:>6} unseen", mailbox.name, mailbox.unseen);
    }
    Ok(())
}

fn list(store: &Store, config: &Config, account: AccountId, args: &Args) -> Result<()> {
    let mailbox = args.mailbox(config, account);
    let envelopes = store.load_envelopes(account, &mailbox, args.limit)?;
    emit(&envelopes, args)
}

/// Filters the cache, or asks the server when `--server` is given.
///
/// The same [`Query`] drives both: `matches` against cached envelopes here,
/// and `to_imap` on the server. What differs is reach, not meaning — except
/// for `body:` and `text:`, which see the whole message on the server and
/// only the cached preview locally.
fn search(store: Store, config: Config, account: AccountId, args: &Args) -> Result<()> {
    let Some(text) = args.rest.first() else { bail!("search needs a query\n\n{USAGE}") };
    let query = Query::parse(text).context("parsing the query")?;
    let mailbox = args.mailbox(&config, account);

    if !args.server {
        // Read from the cache and filter in memory: no account needed, and
        // no round trip.
        let mut matched: Vec<Envelope> = store
            .load_envelopes(account, &mailbox, u32::MAX)?
            .into_iter()
            .filter(|envelope| query.matches(envelope))
            .collect();
        matched.truncate(args.limit as usize);
        return emit(&matched, args);
    }

    let mut engine = engine(config, store)?;
    let generation = 1;
    let envelopes = engine
        .send_and_wait(
            Command::Search {
                account,
                mailbox: mailbox.clone(),
                query: text.clone(),
                scope: args.scope,
                include_spam_and_trash: args.spam_and_trash,
                generation,
            },
            SERVER_TIMEOUT,
            |event| match event {
                Event::SearchResults { envelopes, generation: g, .. } if *g == generation => {
                    Some(envelopes.clone())
                }
                _ => None,
            },
        )
        .context("searching")?;

    emit(&envelopes[..envelopes.len().min(args.limit as usize)], args)
}

/// One message in full: its headers, its text, and what it carries.
///
/// Attachments are described rather than produced — name, type and size. The
/// bytes are in the cache for anything that wants them, and are no use in a
/// line of JSON.
fn show(store: Store, config: Config, account: AccountId, args: &Args) -> Result<()> {
    let Some(uid) = args.rest.first() else { bail!("show needs a UID\n\n{USAGE}") };
    let uid: u32 = uid.parse().with_context(|| format!("{uid:?} is not a UID"))?;
    let mailbox = args.mailbox(&config, account);

    let body = match store.load_raw(account, &mailbox, uid)? {
        Some(raw) => parse::parse_body(&raw),
        // Not cached: the only thing here that reaches the server without
        // being asked to, because a message nobody can read is not an answer.
        None => {
            let mut engine = engine(config, store)?;
            let body = engine
                .send_and_wait(
                    Command::FetchBody { account, mailbox: mailbox.clone(), uid, served: false },
                    SERVER_TIMEOUT,
                    |event| match event {
                        Event::Body { uid: u, body, .. } if *u == uid => Some(body.clone()),
                        _ => None,
                    },
                )
                .with_context(|| format!("fetching message {uid}"))?;
            MessageBody::clone(&body)
        }
    };

    if args.json {
        let text = body.text.clone().unwrap_or_else(|| {
            body.html.as_deref().map(remail::html::strip_tags).unwrap_or_default()
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "mailbox": mailbox,
                "uid": uid,
                "headers": body.headers,
                "text": text,
                "attachments": body.attachments.iter().map(|a| serde_json::json!({
                    "filename": a.filename,
                    "mime": a.mime,
                    "bytes": a.data.len(),
                })).collect::<Vec<_>>(),
                "raw_size": body.raw_size,
            }))?
        );
        return Ok(());
    }

    for (name, value) in &body.headers {
        println!("{name}: {value}");
    }
    println!();
    match (&body.text, &body.html) {
        (Some(text), _) if !text.trim().is_empty() => println!("{text}"),
        (_, Some(html)) => println!("{}", remail::html::strip_tags(html)),
        _ => println!("(no body)"),
    }
    for attachment in &body.attachments {
        println!(
            "\n[attachment] {} ({}, {} bytes)",
            attachment.filename,
            attachment.mime,
            attachment.data.len()
        );
    }
    Ok(())
}

// -- plumbing -------------------------------------------------------------

fn emit(envelopes: &[Envelope], args: &Args) -> Result<()> {
    if args.json {
        println!("{}", serde_json::to_string_pretty(envelopes)?);
        return Ok(());
    }
    for envelope in envelopes {
        let who = envelope.from.first().map(|a| a.short()).unwrap_or("(unknown)");
        let subject = if envelope.subject.is_empty() { "(no subject)" } else { &envelope.subject };
        println!("{:>7}  {:<28.28}  {}", envelope.uid, who, subject);
    }
    Ok(())
}

fn engine(config: Config, store: Store) -> Result<Engine> {
    Engine::start(Arc::new(RwLock::new(config)), Arc::new(store), || {})
        .context("starting the mail engine")
}

/// The cache the interface uses, opened read-write because the engine writes
/// to it. WAL means this is safe alongside a running window.
fn open_store() -> Result<Store> {
    let path = remail::config::data_dir().context("finding the data directory")?;
    Store::open(&path.join("cache.sqlite")).context("opening the message cache")
}

fn pick_account(config: &Config, wanted: Option<AccountId>) -> Result<AccountId> {
    let enabled = || config.accounts.iter().filter(|a| a.enabled);
    match wanted {
        Some(id) => {
            if enabled().any(|a| a.id == id) {
                Ok(id)
            } else {
                bail!("no enabled account with id {id}")
            }
        }
        None => match enabled().count() {
            0 => bail!("no accounts are configured"),
            _ => Ok(enabled().next().expect("counted at least one").id),
        },
    }
}

/// What the command line asked for.
///
/// Hand-rolled rather than pulled from a crate: the grammar is four verbs
/// and seven options, and it is worth more as something a test can hand a
/// list of strings to than as a derive.
#[derive(Debug, Default, PartialEq)]
struct Args {
    verb: String,
    /// Positional arguments after the verb: a query, or a UID.
    rest: Vec<String>,
    account: Option<AccountId>,
    mailbox: Option<String>,
    limit: u32,
    server: bool,
    scope: SearchScope,
    spam_and_trash: bool,
    json: bool,
    help: bool,
}

impl Args {
    fn parse(argv: impl Iterator<Item = String>) -> Result<Self> {
        let mut args = Args { limit: 50, json: true, ..Default::default() };
        let mut argv = argv.peekable();

        // A value-taking option at the end of the line is a mistake worth
        // naming, rather than silently becoming a default.
        let value = |argv: &mut std::iter::Peekable<_>, flag: &str| -> Result<String> {
            match Iterator::next(argv) {
                Some(value) => Ok(value),
                None => bail!("{flag} needs a value"),
            }
        };

        // Set by `--`, after which nothing is read as an option. The query
        // language negates with a leading `-`, so a query is quite often
        // indistinguishable from a mistake without being told.
        let mut positional_only = false;

        while let Some(arg) = argv.next() {
            if positional_only {
                if args.verb.is_empty() {
                    args.verb = arg;
                } else {
                    args.rest.push(arg);
                }
                continue;
            }
            match arg.as_str() {
                "--" => positional_only = true,
                "-h" | "--help" => args.help = true,
                "--server" => args.server = true,
                "--spam-and-trash" => args.spam_and_trash = true,
                "--text" => args.json = false,
                "--json" => args.json = true,
                "--account" => {
                    let raw = value(&mut argv, "--account")?;
                    args.account =
                        Some(raw.parse().with_context(|| format!("{raw:?} is not an account id"))?);
                }
                "--mailbox" => args.mailbox = Some(value(&mut argv, "--mailbox")?),
                "--limit" => {
                    let raw = value(&mut argv, "--limit")?;
                    args.limit = raw.parse().with_context(|| format!("{raw:?} is not a count"))?;
                }
                "--scope" => {
                    let raw = value(&mut argv, "--scope")?;
                    args.scope = match raw.as_str() {
                        "folder" => SearchScope::Folder,
                        "subtree" => SearchScope::Subtree,
                        "all" => SearchScope::All,
                        other => bail!("unknown scope {other:?}: folder, subtree or all"),
                    };
                }
                other if other.starts_with('-') && other != "-" => bail!(
                    "unknown option {other:?} \u{2014} if it is part of a query, put it after \
                     `--`, as in: remail-cli search -- {other:?}"
                ),
                // The first bare word is the verb; the rest belong to it.
                _ if args.verb.is_empty() => args.verb = arg,
                _ => args.rest.push(arg),
            }
        }

        if args.verb.is_empty() && !args.help {
            args.help = true;
        }
        Ok(args)
    }

    /// The mailbox to work in: the one asked for, else the account's own
    /// default, else INBOX.
    fn mailbox(&self, config: &Config, account: AccountId) -> String {
        if let Some(mailbox) = &self.mailbox {
            return mailbox.clone();
        }
        config
            .accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.default_mailbox.clone())
            .filter(|mailbox| !mailbox.is_empty())
            .unwrap_or_else(|| "INBOX".to_string())
    }
}

const USAGE: &str = "\
remail-cli — read a remail mailbox from a command line

  folders                       the mailboxes on file
  list                          envelopes from the cache
  search <query>                filter the cache, or --server to ask IMAP
  show <uid>                    one message in full

Options
  --account <id>                which account (default: the first enabled)
  --mailbox <name>              which mailbox (default: the account's)
  --limit <n>                   how many to return (default: 50)
  --server                      run the search on the server
  --scope folder|subtree|all    how far --server reaches (default: folder)
  --spam-and-trash              include Spam and Trash in an 'all' search
  --text                        human-readable output instead of JSON
  -h, --help                    this
  --                            everything after this is not an option,
                                for a query that starts with '-'
";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Args> {
        Args::parse(line.split_whitespace().map(str::to_string))
    }

    #[test]
    fn the_defaults_are_the_cheap_and_safe_ones() {
        let args = parse("search from:dupr").unwrap();
        assert_eq!(args.verb, "search");
        assert_eq!(args.rest, vec!["from:dupr"]);
        assert!(!args.server, "a search went to the server without being asked");
        assert_eq!(args.scope, SearchScope::Folder);
        assert!(args.json, "output is for a program unless --text says otherwise");
        assert_eq!(args.limit, 50);
    }

    #[test]
    fn options_are_read_wherever_they_appear() {
        let before = parse("--server --limit 5 search from:dupr").unwrap();
        let after = parse("search --server from:dupr --limit 5").unwrap();
        assert_eq!(before, after, "an option's meaning changed with its position");
        assert!(after.server);
        assert_eq!(after.limit, 5);
        assert_eq!(after.rest, vec!["from:dupr"], "the query was eaten by an option");
    }

    #[test]
    fn the_scopes_are_named_as_they_are_written() {
        assert_eq!(parse("search --scope folder x").unwrap().scope, SearchScope::Folder);
        assert_eq!(parse("search --scope subtree x").unwrap().scope, SearchScope::Subtree);
        assert_eq!(parse("search --scope all x").unwrap().scope, SearchScope::All);

        let wrong = parse("search --scope everywhere x").unwrap_err().to_string();
        assert!(wrong.contains("everywhere"), "{wrong}");
    }

    #[test]
    fn text_turns_the_json_off_and_json_turns_it_back_on() {
        assert!(!parse("list --text").unwrap().json);
        assert!(parse("list --text --json").unwrap().json);
    }

    /// A mistake is worth naming. Silently defaulting would give an answer
    /// to a question other than the one asked.
    #[test]
    fn a_malformed_option_is_refused() {
        for line in [
            "search --limit",            // no value
            "search --limit lots",       // not a number
            "search --account",          // no value
            "search --account everyone", // not an id
            "search --mailbox",          // no value
            "list --colour",             // no such option
        ] {
            assert!(parse(line).is_err(), "{line:?} was accepted");
        }
    }

    #[test]
    fn nothing_at_all_asks_for_help() {
        assert!(parse("").unwrap().help);
        assert!(parse("--help").unwrap().help);
        assert!(parse("-h").unwrap().help);
    }

    /// The query language negates with a leading `-`, so a perfectly good
    /// query looks exactly like a mistyped option. `--` says which it is.
    #[test]
    fn a_query_starting_with_a_dash_survives_after_a_terminator() {
        let args = parse("search -- -is:read").unwrap();
        assert_eq!(args.verb, "search");
        assert_eq!(args.rest, vec!["-is:read"]);

        // And options before it are still options.
        let args = parse("search --limit 5 -- -is:read").unwrap();
        assert_eq!(args.limit, 5);
        assert_eq!(args.rest, vec!["-is:read"]);
    }

    /// Without the terminator it is a mistake, and the complaint says how to
    /// spell it if it was not.
    #[test]
    fn a_bare_leading_dash_is_refused_with_the_remedy() {
        let refused = parse("search -is:read").unwrap_err().to_string();
        assert!(refused.contains("-is:read"), "{refused}");
        assert!(refused.contains("--"), "the complaint did not say what to do: {refused}");
    }

    #[test]
    fn the_mailbox_falls_back_through_the_account_to_inbox() {
        let mut config = Config::default();
        config.accounts.push(remail::config::AccountConfig::imap(1, "me@example.com"));

        let asked = parse("list --mailbox Archery").unwrap();
        assert_eq!(asked.mailbox(&config, 1), "Archery");

        let unasked = parse("list").unwrap();
        assert_eq!(unasked.mailbox(&config, 1), "INBOX", "the account's own default");

        // An account that is not in the configuration at all.
        assert_eq!(unasked.mailbox(&config, 99), "INBOX");
    }
}
