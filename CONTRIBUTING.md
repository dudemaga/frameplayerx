# Contributing to FramePlayerX

FramePlayerX is maintained by one person, and every feature, setting and button is something to keep working from then on. These guidelines keep contributions useful and reviews manageable.

You don't need to write code to help:

- **Found a bug or have an idea?** Open a [bug report](https://github.com/remotesojourner/frameplayerx/issues/new?template=bug_report.md) or a [suggestion](https://github.com/remotesojourner/frameplayerx/issues/new?template=feature_request.md).
- **Spotted a wrong translation?** See [Help translate](README.md#help-translate).

## Before you write code

1. **Read the issues, open and closed.** They record what is planned, what has been tried and what was rejected. If one covers your change, do what it asks. If you think it's wrong, say so there first, with your evidence.
2. **Open an issue for anything new, and wait for a yes.** That means a feature, a setting, a button or a change to how something behaves. A fix for a clear bug can go straight to a pull request.
3. **Say what changes for someone wearing the headset,** and what already covers it. A feature that repeats what the controllers, the phone remote or an existing setting already do won't be added.
4. **Keep it simple.** Controls do what their name says. Prefer making something work better over adding a setting: a setting asks the person in the headset to diagnose the problem.

## Pull requests

- **One change per pull request, one at a time.** Wait for a review before opening the next.
- **Start from the current `release/x.y` branch,** and target it. If there isn't one, use `main`. Keep your branch free of conflicts.
- **Link the issue** it addresses.
- **Check every claim in the description.** If it says something doesn't get worse, you measured that.

The description has to cover:

| Section | What to say |
|---|---|
| Problem | What you saw, and on what |
| Approach | What you chose, what you ruled out and why. If it departs from the issue, how and why |
| Costs | What gets slower, larger or riskier, and who that affects. Almost every change costs something |
| Testing | What you ran, on what, and the results. What you did not test |

## Testing

The checks in [Build and test](docs/DEVELOPMENT.md#build-and-test) have to pass, and CI runs them. That is where testing starts, not where it ends:

- **Test on a Steam Frame** anything that touches playback, decoding, rendering or controls; see [On the headset](docs/DEVELOPMENT.md#on-the-headset). If you can't, say so, and expect the change to wait until someone can.
- **Test beyond your own setup.** One file on one server shows that a change can work, not that it does. Try what it could make worse: slow connections as well as fast ones, small videos as well as 8K, the other kinds of source.
- **Test what people do around it.** Opening, skipping, pausing and going back, and what happens when something fails or is refused, not only the path you set out to fix.
- **Give before-and-after numbers for performance changes,** from the real player on the headset, and say how you measured them. Playback there is limited by the GPU, so a headless benchmark can look fine while the player gets worse.
- **Be careful with anything outside the player.** Changing, closing or deleting something the player doesn't own needs an issue first, and must only ever act on what it has positively identified.

## Code and docs

- Follow the [code style](docs/DEVELOPMENT.md#code-style).
- Add interface text to the English file only. Other languages come through [Codeberg Translate](docs/DEVELOPMENT.md#translations).
- If behaviour changes, update the [user guide](docs/USER-GUIDE.md). Don't add new documentation files.

## AI tools

AI tools are welcome: FramePlayerX itself is [built with extensive help from Claude Code](README.md#a-note-on-ai). They don't change what is expected of the person submitting the work:

- You have read all of it, and can explain it and answer questions about it yourself.
- You made the design decisions: you questioned the first answer, weighed the alternatives and checked it against the issues and this guide.
- You tested it yourself, on real hardware. Passing tests that the tool wrote are not enough.
- You checked every claim in the description, even if the tool wrote it.

## What to expect

Pull requests that skip these steps, or that read as unreviewed tool output, may be closed without a detailed review. That isn't a judgement of you: one person can't review everything. If the idea is useful, it may be picked up later, with credit to you.

Contributions are licensed under the Apache License, Version 2.0, as described in the [README](README.md#license).
