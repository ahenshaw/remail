# remail

A fast IMAP and Gmail client in Rust, with an egui interface and an optional
Servo rendering backend for message bodies.

## What it does

- **Gmail via OAuth2** (`XOAUTH2` over IMAP and SMTP) and **any IMAP server**
  with a password or app-password.
- **Instant folder switching.** Mailboxes, envelopes and message bodies are
  cached in SQLite (WAL). Opening a folder paints from cache in the same frame;
  the server reconciliation arrives moments later.
- **Push mail.** A second connection parks in `IDLE` per account, so new mail
  arrives without polling. Servers without `IDLE` fall back to a timer.
- **Two HTML renderers.** A built-in one that draws sanitized mail directly with
  egui, and Servo for messages that need real CSS layout.
- **Per-pane typography.** Folders, messages and the reading column each pick
  their own size and font from any family installed on the system.
- **Private by default.** Remote images are blocked until you ask for them,
  per message.
- **Print** hands the message to your browser, where print preview, page setup
  and PDF export already live.

## Building

```sh
cargo run --release
```

With the Servo backend:

```sh
# Servo builds SpiderMonkey from source: expect ~1 hour and ~20 GB the first time.
LIBCLANG_PATH=/usr/lib/llvm-18/lib cargo run --release --features servo
```

`LIBCLANG_PATH` is only needed if `bindgen` cannot find `libclang.so` on its
own. Requires `clang`, `cmake`, `ninja` and `python3`.

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

## Keyboard

| Key | Action | | Key | Action |
|---|---|---|---|---|
| `j` / `↓` | next message | | `e` | archive |
| `k` / `↑` | previous message | | `Del` | delete |
| `r` | reply | | `u` | toggle read |
| `R` | reply all | | `s` | toggle star |
| `f` | forward | | `F5` | sync mailbox |
| `c` | compose | | `Esc` | clear search |

`Ctrl+Enter` sends from the compose window. Typing in the search box filters
the cached listing; pressing `Enter` escalates to a server-side `SEARCH`, which
reaches messages that were never cached.

The scope selector beside the search box controls how far `Enter` reaches:
this folder, this folder and everything nested under it, or the whole account.
Base IMAP has no cross-folder search, so wider scopes select and search each
mailbox in turn — except on Gmail, where `All Mail` already contains every
message and one search covers the account. Every result shows which folder it
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
    servo.rs        drive Servo offscreen (feature = "servo")
  ui/               sidebar, message list, reader, compose, dialogs
```

The UI never touches the network and never blocks. It sends commands to the
mail engine and drains events once per frame; everything on screen is a
projection of state those events left behind.

### The two renderers

Both consume the *same* sanitized document, so security does not depend on
which is selected.

The **built-in** renderer parses sanitized HTML into a small block model
(paragraphs, headings, lists, quotes, tables, images) and draws it with egui
widgets. Message text is selectable, follows the app theme, and costs nothing
when idle. It honours inline `style` colour, weight, and decoration, treats
one-column tables as the layout scaffolding they usually are, and falls back
to the theme's text colour when a sender's colour would be invisible against
the current background. It does not implement the CSS cascade, floats, or
flexbox — heavily art-directed mail gets linearized into readable content.

The **Servo** backend covers that gap. The body is wrapped in a themed
document, loaded from a `data:` URL (opaque origin, no ambient authority),
rendered into a `SoftwareRenderingContext`, and read back as an egui texture.
Servo is pumped from the UI thread, which is where it must run.

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
  removed before either renderer sees the markup.
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
cargo test                 # unit tests
cargo test -- --ignored    # adds a live TLS/greeting/LOGIN check against Gmail
```
