# remail

[![CI](https://github.com/ahenshaw/remail/actions/workflows/ci.yml/badge.svg)](https://github.com/ahenshaw/remail/actions/workflows/ci.yml)

A fast IMAP and Gmail client in Rust, with an egui interface and a built-in
renderer for message bodies.

## What it does

- **Gmail via OAuth2** (`XOAUTH2` over IMAP and SMTP) and **any IMAP server**
  with a password or app-password.
- **Instant folder switching.** Mailboxes, envelopes and message bodies are
  cached in SQLite (WAL). Opening a folder paints from cache in the same frame;
  the server reconciliation arrives moments later.
- **Push mail.** A second connection parks in `IDLE` per account, so new mail
  arrives without polling. Servers without `IDLE` fall back to a timer.
- **Sanitized HTML, drawn natively.** Message bodies are stripped of active
  content and laid out with egui, so mail is selectable, themed, and costs
  nothing when idle.
- **Per-pane typography.** Folders, messages and the reading column each pick
  their own size and font from any family installed on the system.
- **Private by default.** Remote images are blocked until you ask for them,
  per message.
- **Open in your browser** for mail built as a page, which the built-in
  renderer linearizes rather than lays out. The browser gets the same
  sanitized document, still withholding whatever the reader was withholding.
  Print preview, page setup and PDF export are there too.
- **Recipient completion** from every address the account has seen, ranked so
  the people you write to come before the lists that write to you.
- **A search language**: `subject:invoice from:jane -is:read since:2w`. The
  same query filters what is loaded as you type and compiles to an IMAP
  `SEARCH` when you press Enter.
- **Move to a folder** by dragging onto the sidebar — one message or a whole
  selection, carried as a translucent copy of the rows themselves. Or from
  the toolbar, `m`, or a right-click, which open a folder picker you filter
  by typing. Rows leave the list at once and come back if the server
  refuses.
- **Send as** any address configured for the account. A reply goes out from
  whichever of them the message was addressed to.

## Building

```sh
cargo run --release
```

There is nothing to configure and no optional feature to pick. On Linux the
usual `eframe` libraries are needed at link time: `libxcb-render`,
`libxcb-shape`, `libxcb-xfixes`, `libxkbcommon` and GL headers.

## Installing

Prebuilt packages for each tagged version are on the
[releases page](https://github.com/ahenshaw/remail/releases): a `.deb` for
Debian and Ubuntu, and an installer for Windows.

To build and install from source instead:

```sh
cargo build --release

# Debian, Ubuntu: a package, with dependencies and a man page
./packaging/linux/build-deb.sh
sudo dpkg -i dist/remail_*.deb

# Anywhere else: straight into ~/.local, no root needed
./packaging/linux/install.sh
```

Both put the desktop entry and the icon where the desktop environment looks
for them. `install.sh --system` installs to `/usr/local` instead, and
`install.sh --uninstall` reverses whichever it did.

The Windows installer is built from `packaging/windows/remail.iss` with
[Inno Setup](https://jrsoftware.org/isinfo.php):

```
cargo build --release
iscc packaging\windows\remail.iss
```

The icons all come from one master, `assets/remail.svg`. Regenerate the PNGs
and the `.ico` with `./scripts/icons.sh` after editing it.

## Adding an account

**Any IMAP server** — Accounts → `+ IMAP`, fill in the address and password.
Hosts default to `imap.<domain>` / `smtp.<domain>`; correct them if needed.

**Gmail** — Google does not publish an OAuth client that third-party
applications may share, so you register your own:

1. In [Google Cloud Console](https://console.cloud.google.com/apis/credentials),
   create an OAuth client of type **Desktop app**.
2. Enable the Gmail API for the project and add your address as a test user.
3. Accounts → `+ Gmail`, paste the client id and secret, Save, then
   **Sign in with Google**. A browser opens; the code comes back over a
   loopback listener with PKCE.

Gmail also works with an app password via `+ IMAP` if you would rather skip
OAuth entirely.

Passwords and refresh tokens go to the OS keyring, never to the config file.
On Linux that needs a Secret Service (GNOME Keyring, KWallet); the accounts
dialog says so plainly if none is running.

## Search

Typing filters the messages already loaded. Enter escalates to a server-side
search, over the folder, its subfolders, or the whole account, depending on
the scope selector beside the box.

Both understand the same small query language. Plain words search subject,
sender and recipient together, and terms are ANDed:

```
subject:invoice from:jane
```

| | |
|---|---|
| `subject:` `from:` `to:` `cc:` `bcc:` | header fields |
| `body:` `text:` | the message body; `text:` is headers and body together |
| `is:unread` `is:read` `is:starred` `is:answered` `is:draft` | state |
| `has:attachment` | |
| `since:` `before:` `on:` | `2026-01-31`, or an offset: `7d` `2w` `3m` `1y` |
| `larger:` `smaller:` | `200k`, `2m`, or a byte count |
| `"two words"` | a phrase |
| `-term` | exclude |
| `OR`, `( )` | alternatives and grouping; AND binds tighter than OR |
| `AND` | optional: the space between terms already means it |

So `subject:report -from:noreply since:2w` is three terms, and
`(from:jane OR from:paul) has:attachment` is two. `subject:pickleball AND
from:dupr` is the same query as `subject:pickleball from:dupr`; write the
conjunction or leave it out. To search for the words themselves, quote them:
`"and"`.

Two places where the filter and the search differ, both because the cache
holds envelopes rather than whole messages:

- `body:` and `text:` match only the cached preview while you type, and the
  whole message once you press Enter. The list says so when that is why it
  came up empty.
- `has:attachment` is exact locally, because the structure was parsed when
  the envelope was cached. On the server it compiles to a `Content-Type`
  header test, which is the closest IMAP can answer: it misses attachments
  sent as `multipart/related`, and matches some messages that only have an
  inline image.

A malformed query is reported in the status line rather than searched for, so
`colour:red` says that `colour:` is not a field instead of quietly finding
nothing.


## Keyboard

| Key | Action | | Key | Action |
|---|---|---|---|---|
| `j` / `↓` | next message | | `e` | archive |
| `k` / `↑` | previous message | | `m` | move to a folder |
| `r` | reply | | `Del` | delete |
| `R` | reply all | | `u` | toggle read |
| `f` | forward | | `s` | toggle star |
| `c` | compose | | `F5` | sync mailbox |
| | | | `Esc` | clear search |

Messages can also be dragged onto a folder in the sidebar, individually or as
a selection. A folder lights up only if it will actually take the drop: one
that holds no messages, one the messages are already in, and any folder
belonging to a different account stay inert, since a move is one IMAP session
acting on one server. Resting on a collapsed folder opens it, so a nested
destination can be reached without breaking off the drag; anything opened
that way closes again when the drag ends, and the config never hears about
it.

`Ctrl+Enter` sends from the compose window. Typing in the search box filters
the cached listing; pressing `Enter` escalates to a server-side `SEARCH`, which
reaches messages that were never cached.

The scope selector beside the search box controls how far `Enter` reaches:
this folder, this folder and everything nested under it, or the whole account.
Base IMAP has no cross-folder search, so wider scopes select and search each
mailbox in turn — except on Gmail, where `All Mail` stands in for most of the
account. Not all of it: RFC 6154 permits an `\All` mailbox to omit `\Trash`
and `\Junk`, and Gmail does. A toggle beside the scope selector says whether
those two belong in the results; it appears only for a whole-account search,
where it is the only scope the choice can apply to. Every result shows which folder it
came from, and acting on one targets the right mailbox for that row.

On Gmail the folder shown comes from the message's labels rather than the
mailbox it was found in: a whole-account search runs against All Mail, which
is the union of every label rather than a place.

## How it is put together

```
src/
  app.rs            application state, event pump, window layout
  config.rs         accounts and settings (TOML)
  secrets.rs        OS keyring
  auth/             OAuth2: PKCE, loopback redirect, token refresh
  mail/
    engine.rs       async supervisor + one worker per account
    imap.rs         connection, sync, IDLE, flags, moves, search
    smtp.rs         message building and sending
    store.rs        SQLite cache
    parse.rs        MIME -> model
  html/
    sanitize.rs     strip active content, block trackers
    dom.rs          HTML parser
    layout.rs       lower to a block model
    native.rs       draw the block model with egui
    print.rs        build a standalone document for the browser
  ui/               sidebar, message list, reader, compose, dialogs
```

The UI never touches the network and never blocks. It sends commands to the
mail engine and drains events once per frame; everything on screen is a
projection of state those events left behind.

### The renderer

The renderer parses sanitized HTML into a small block model (paragraphs,
headings, lists, quotes, tables, images) and draws it with egui widgets.
Message text is selectable, follows the app theme, and costs nothing when
idle. It honours inline `style` colour, weight, and decoration, treats
one-column tables as the layout scaffolding they usually are, and falls back
to the theme's text colour when a sender's colour would be invisible against
the current background.

It does not implement the CSS cascade, floats, or flexbox — heavily
art-directed mail gets linearized into readable content. When that loses
something, **Browser** hands the message to one that does have a full engine.
The document it opens is the sanitized one, with `cid:` parts inlined so it
stands on its own, and with remote content still blocked if the message is
still blocking it — so opening it discloses nothing that reading it had not.

### Sync

Per mailbox the client tracks `UIDVALIDITY` and the highest UID it has seen.
A sync fetches headers only for UIDs above that mark, then re-reads flags
across the cached window so reads and stars made in another client show up,
and notices messages that disappeared. Bodies are never fetched during sync —
they are pulled on demand and prefetched a few rows either side of the
viewport.

A changed `UIDVALIDITY` invalidates every cached UID, so the mailbox cache is
dropped and rebuilt.

## Security notes

- `<script>`, `<style>`, `<iframe>`, event handlers and `javascript:` URLs are
  removed before the renderer sees the markup.
- Remote images are withheld by default; the reader says how many and offers to
  load them, because loading them tells the sender you opened the message.
  The choice is remembered, at either of two scopes: **Load images** applies to
  that message alone, and **Always from sender** applies to everything from
  that address. They are separate buttons because they differ in what they
  give away — the first reveals only an open you already performed, the second
  reveals future opens before you have decided on them. Settings shows how many
  senders are trusted and revokes them all in one click.
- Only `http`, `https`, `mailto`, `tel`, `cid` and `data` URLs survive
  sanitization, and only those schemes are handed to the system browser.
- Attachment filenames are stripped of path separators before saving, and
  saving never overwrites an existing file.
- Message bodies are fetched with `BODY.PEEK[]`, so reading does not implicitly
  set `\Seen` — that is the client's decision, on its own timer.

## Testing

```sh
cargo test                      # unit tests
cargo test -- --ignored         # adds a live check against Gmail
cargo clippy --all-targets      # CI runs this with -D warnings
cargo fmt --check
python3 scripts/glyphs.py       # every drawn icon exists in egui's fonts
```

CI runs all of these on push and pull request; see
[`.github/workflows/ci.yml`](.github/workflows/ci.yml). The live Gmail check
stays `#[ignore]`d, since it needs credentials.

## License

MIT. See [LICENSE](LICENSE).
