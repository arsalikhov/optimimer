# Using the bot

## First contact

After you send the setup code the bot runs a short onboarding:

1. **Name** — "What should I call you?" (one-tap default: your Telegram name).
2. **Timezone** — share a location pin (📎 → Location) or type an IANA name; one tap keeps the current one.
3. **Categories** (owner only) — keep the defaults (Admin, Work, Fitness, Home, Finance, Learning, Social, Travel,
   Personal) or send your own as `Name: what belongs there, Name: …`. The last one is the catch-all. Projects nest
   under categories, and the bot may create projects but never categories.
4. **Models** (owner only) — free (default; no OpenRouter credits needed) or paid (Claude + Voxtral, best
   results). The bot shows your OpenRouter balance. Switch later with "use paid models" / "use free models".
5. **Machine** (owner only, optional) — `name MAC [interface]` for Wake-on-LAN, or Skip.

Say "run setup again" any time to repeat it, or "show settings" to see what is stored.

## Just talk

There are no slash commands. Every message, typed or spoken, goes to one tool-calling model that acts and replies in
a sentence or two. Anything tabular (balance, transactions, lists, search results) is shown directly as a rich
message; destructive actions (clearing a list, deleting a transaction, replacing categories) get Yes/Cancel buttons.

What you can ask for, by area:

| Area | Examples |
| ---- | -------- |
| Tasks | "add a task: renew passport by friday", "what's open?", "done with the passport", "find the dentist task" |
| Notes & memos | "note: ideas for the trip …", "list notes", "search notes", "what does the trip note say?" |
| Editing | "add to the trip note that the flight is booked", "retitle it Bike fit", "that summary says Tuesday" |
| Money | "spent 12.50 on lunch", "earned 3000 salary", "balance", "transactions", "remove transaction 3" |
| Income | "my monthly income is 4000" |
| Shopping | "add milk and eggs", "groceries", "shopping list", "clear the grocery list" |
| Reminders | "remind me tomorrow at 9 to call the bank", "in 20 minutes remind me about the oven" |
| Email | "email Sam: running 10 minutes late" (needs Resend, see configuration) |
| Stock watches | "watch <url>", "my watches", "stop watching 2" |
| Machines | "wake the desktop", "add machine nas aa:bb:cc:dd:ee:ff", "remove machine nas" |
| Memory | "remember Sam prefers mornings", "what do you know about Sam?", "no, Sam moved to Lisbon", "forget that" |
| Recovering | "what's in the bin?", "restore Sam", "you forgot that by mistake" |
| Context | "clear context", "new chat", "start over" — a fresh thread; memory and files stay |
| Models | "escalate to unsafe", "use opus", "switch to sonnet" pin the chat; "back to normal" unpins |
| Web | "what's the weather in Lisbon tomorrow?", "when does the Apple store close today?", "summarise https://…" |
| Settings | "call me Alex", "my timezone is Europe/Berlin", "show settings", "run setup again", "use paid models" |
| People | "invite someone", "remove Sam" |
| Voice | "for voice notes, learn the word Kubernetes" |

Dates and times are resolved in code, in your timezone: "next tuesday 2pm" becomes a real timestamp, and the
default time for a task without one is 16:00.

## Voice

Send a voice message and it is transcribed (via OpenRouter) and handled like text. It becomes a **memo** instead —
transcribed, summarised and saved to `summaries/` in the vault — when it opens with "memo", "note to self" or "voice
note", when it is 45 seconds or longer (`VOICE_MEMO_SECS`), or when it is short but the bot cannot make a request
out of it. Words the transcriber keeps mishearing can be taught: "for voice notes, learn the words Aqusense and
Kubernetes".

Whenever a recording is *kept* — as a note, a memo or a forwarded batch — the raw transcript is filed under
`transcripts/` in the vault and linked to the tidied version, so you can always check what was actually said. A
spoken command is not kept: ask it to add milk to the list and nothing but the list changes.

## Forwarded conversations

Forward a batch of messages from any chat. Once they stop arriving the bot replies with a title, key points, action
items and the full transcript folded into a collapsible block, and saves it to `summaries/`. Add a comment when you
forward and it is used as the brief ("what did we decide about the venue?"); without one the bot asks, and you can
reply or tap *Summarize as-is*. Forwarded voice messages are transcribed into the transcript.

## Photos

Send a picture and a vision model (`OCR_MODEL`) reads it: what it shows, plus any text on it transcribed verbatim.
That reading becomes an ordinary turn for the bot, so a photo is context like anything else you type. Add a caption
to say what you want done with it:

- "save this" on a whiteboard, a slide or a page of handwriting → a note with the text written out.
- "put this in the calendar" on a flyer or an invitation → a task with the date and place off the picture.
- "remember this" on a business card or a label → the details go into the memory graph.
- no caption → the bot decides: a receipt is logged, something with a date in it becomes a task, a page of text
  becomes a note, and anything else gets a sentence about what it sees and an offer to keep it.

Whenever a photo is *kept* — as a note, a task or a summary — the image itself is filed under `attachments/` in the
vault and embedded at the top of that file, so the note shows the picture it came from. A photo you only asked a
question about is not kept. Forwarded pictures are read the same way and go into the batch summary.

## Receipts and bank statements

- A **photo** of a receipt sent on its own is read and logged as an expense (amount, merchant, date) — no caption
  needed. With a caption it goes to the bot instead, which still gets the amount and can do what you asked.
- A **CSV** bank or card statement is imported: rows are classified (expense, income, transfer, refund), de-duplicated
  by date + amount + payee, and card payments or salary are never double-counted. See [Finance](finance.md).

## Where things end up

| You said | Where it goes |
| -------- | ------------- |
| tasks, notes, memos, summaries | Markdown files in the vault (`tasks/`, `notes/`, `summaries/`) |
| voice kept as a note or summary | the words as transcribed, in `transcripts/`, linked to it |
| a photo kept as a note, task or summary | the image in `attachments/`, embedded in that file |
| money | the SQLite ledger, mirrored to `finance/` notes and `Charts.md` |
| facts, people, preferences | the memory graph, mirrored to `memory/` notes |
| lists, reminders, watches, settings | SQLite only |
