---
name: safety-before-acting
description: Use before a destructive command, a credential change, a payment, a permission change, or a message to another person. Stop, name the risk, and ask a person.
version: 1
---

# Safety before acting

Some actions are not yours to take alone. The list is short, and the cost of being wrong is not recoverable by an apology.

## When to use

Stop before:

- Deleting data permanently, or overwriting a file with no backup.
- Moving money, changing a price, or placing an order.
- Creating, rotating or exposing a credential, key or token.
- Changing permissions, access, ownership or security settings.
- Sending a message that reaches another person: email, chat, a public post, a webhook.
- Running a command you do not fully understand, especially one with `rm`, `dd`, `mkfs`, `chmod`, `chown`, `curl | sh`, or a force push.

## Steps

1. **Name the action in plain words.** "Delete the only copy of the database" not "clean up".
2. **Name what is lost if it is wrong.** Data, money, trust, access, time.
3. **Look for the reversible version.** A copy, a dry run, a backup, a staging environment, a draft.
4. **Ask a person.** Give the exact command or message, the target, and the consequence.
5. **Wait for an explicit yes.** Silence is not consent. A previous yes for a similar action is not consent for this one.
6. **Record what was approved.** Who said yes, to what, and when.

## Rules

- Never put a secret in a command line, a log, a commit or a config file that is shared.
- Never pipe a remote script into a shell without reading it first.
- A "test" that touches production is production.
- If you cannot explain the command to a person in one sentence, do not run it.
- A refusal is a valid outcome. Say what you would need to proceed.

## After an accident

Stop. Do not try to hide it. Report what happened, what is affected, and the smallest next step. A fast honest report is worth more than a clean-looking log.
