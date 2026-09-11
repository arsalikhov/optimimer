# Finance

Money lives in a `transactions` table in the SQLite database; all arithmetic happens in Rust, never in the model.

## Logging

- "spent 12.50 on lunch at Joe's" → expense with merchant, category (Groceries, Dining, Transport, Housing,
  Utilities, Health, Entertainment, Shopping, Subscriptions, Travel, Loans, Cash, Other) and date.
- "earned 3000 salary" → income; regular pay is category Salary.
- A **receipt photo** is OCR'd (`OCR_MODEL`) into the same form.
- "my monthly income is 4000" sets the figure the balance view compares against.

## CSV import

Send a bank or card statement as a `.csv` document. The importer detects the account type from the filename and
header, classifies each row as expense / income / transfer / refund (`FINANCE_MODEL`), and de-duplicates by a
date + amount + payee fingerprint. Card bill payments and transfers between your own accounts are recorded as
transfers (Card payment, Savings, Transfer in, Transfer out) so they never count as spending or income. Repeated
imports of the same file hit a cache and do not call the model again. A same-amount near-match within a few days is
flagged ⚠️ in the transaction list for you to review; "remove transaction 3" deletes it after a confirmation.

`RENT_AMOUNTS` pins exact e-transfer amounts to Housing; a few well-known payees are pinned deterministically too.

## Balance and history

"balance" shows this month's income, spending by category and net; "transactions" lists recent rows with numbers you
can refer to. `Finances.base` in the vault has views for this month, by month, spending by category, transfers and
flagged duplicates; `Charts.md` and the Sankey on `Home.md` are regenerated after every change (see
[The vault](vault.md)).

## Starting clean

Set `FINANCE_PLACEHOLDERS=1` to seed an empty ledger with labelled sample rows (source `Placeholder`) for the last
three months so every view renders. They vanish the moment a real expense, receipt or CSV row arrives.

## Migrating from Notion

Earlier versions stored everything in Notion. `scripts/notion_export.py` (standard library only) dumps every
database to JSON plus one Markdown file per page for archiving; nothing is imported into the vault.
