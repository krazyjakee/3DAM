# 3DAM — Mission Statement

**3DAM** (pronounced "three-dee-A-M") is a free, open-source asset manager for game
developers. It unifies **sound**, **image**, and **3D model** libraries into one fast,
searchable database, and uses content-aware automation to do the tedious cataloguing work
for you.

## The problem

Game asset libraries are sprawling, heterogeneous, and badly organised. A single project
touches thousands of audio clips, textures, sprites, and meshes — spread across local
drives, network shares, and vendor packs downloaded over years. The tools that exist today
are fragmented and compromised:

- They specialise in **one media type** (a sample browser *or* a texture manager *or* a 3D
  catalogue), forcing developers to juggle several apps with several databases.
- The good ones are **proprietary, cloud-locked, or subscription-walled** — your metadata
  lives on someone else's server, and your workflow stops when your card expires.
- Organisation is **manual**. Tagging ten thousand files by hand is nobody's job, so it
  never happens, and search degrades to guessing filenames.

## Our answer

One tool, three media types, one database, no subscription, no cloud lock-in.

3DAM takes the best idea from each category-leading app and puts them under one roof:

- **Content-based analysis and similarity search** for audio — inspired by sample browsers
  like Sononym — extended to images and 3D models.
- **A unified asset browser** with a rich metadata panel — in the spirit of Connecter —
  that treats a `.wav`, a `.png`, and a `.glb` as first-class citizens of the same library.
- **A convert / compress / optimize pipeline** and preview generation — as seen in tools
  like echo3D — running locally and scriptably.

## Principles

1. **Speed is a feature.** Scanning, indexing, previewing, and searching must feel instant
   on libraries of hundreds of thousands of assets. Native Rust, no runtime tax.
2. **Automation over data entry.** The computer looks at the *content* — the waveform, the
   pixels, the geometry — and proposes categories, tags, and duplicates. Humans confirm;
   they don't type.
3. **Your data is yours.** Open format, local-first database, plain-text exports. You can
   leave at any time and take everything with you.
4. **One binary, every role.** A single executable is the desktop GUI, the CLI, *and* the
   server. Anything the app can do, the CLI can do; and `3dam serve` turns that same binary
   into a self-hosted server with a web client — so a library can live on your workstation
   or NAS and be browsed from anywhere on your network, with nothing to install.
5. **Meet assets where they live.** Local disks, SFTP, SMB/Samba, and other standard
   network shares are all just sources. No forced import, no forced copy — and a *another
   3DAM server* is a source too: connect one and its catalogue becomes an extension of your
   own, searched side by side with your local library.
6. **Federate, don't centralise.** Querying a remote catalogue means asking the instance
   that built it — not copying or re-processing its files. From that primitive, communities,
   creators, and stores can host their own authenticated instances that compose into large
   shared asset networks, with no central owner.
7. **Free and open, permanently.** MIT-licensed. No paid tier, no telemetry, no gate.

## Who it's for

Indie developers, small studios, tech artists, sound designers, and modders who own large,
messy, multi-format asset libraries and want to *find things fast* without paying rent or
surrendering their catalogue to a cloud.

## What success looks like

A developer points 3DAM at a decade of accumulated asset packs across three drives and a
NAS, walks away for coffee, and comes back to a browsable, auto-categorised, de-duplicated
library where "find me more footstep sounds like this one" and "show me every low-poly rock"
are one click — or one CLI command — away.

---

See also: [DESIGN_GUIDELINES.md](DESIGN_GUIDELINES.md) · [PRODUCT_SPEC.md](PRODUCT_SPEC.md)
