# HANDOFF

## 1. Setup
Nothing to do for now.

## 2. Tasks (see below)
Then I want you to perform the tasks not yet done in below Tasks section.
For that you need to clone https://github.com/nsauzede/lore.git and checkout branch f-drive.

## 3. Check
Nothing to do for now.

## 4. Update project
Then I want you to update the present HANDOFF.md + any other relevant documents to reflect the current state of the project,
for your future self to take over new tasks I'll append here (mark as done those which are to keep this HANDOFF clean & maintainable).
The step 1 (eg: Rust, maybe Sveltekit) shall always be instructed because of peculiar AI's sandbox constraints wrt Rust.
Eg: if you face any crates version issue in Cargo.toml, please find a working crate set and update it too. Use "=x.y" if required etc.
Add all your work in a new git commit (use your identity !) then `cargo clean` and create a tarball of the whole project + git history and present
it as downloadble tarball.
Don't hesitate to enhance this HANDOFF.md if need be.

# Tasks
- [ ] add a new task here that instructs a future session how to add a simple `lore-drive` bin app to the workspace in `lore-drive/`,
      that's right: right here, I already added a mock Cargo.toml to show you the way - it should probably be also referenced in upper workspace: you will instruct that as well;
      what it's supposed to be: a simple Rust axum/tokio backend that acts as a lore client: you run it at the same place where
      woulf use its fellow bin app `lore` client to interact with a local copy of a distant `server` server project.
      as a backend, it serves a minimalistic REST API eg: on port 8080 for a frontend (out of scope for now) to display
      a browsable files/folders tree of the current workdir (start at root, breadcrumb etc)
      show item name/UUID and additionally size/native content hash for files. I want for the frontend to present exact same IDs/Hashes etc
      as are stored in the internal CAS, 1-to-1.
      so the very output for this first task: a rigorous REST_API.md + directives in HANDOFF next task to create the new app scaffolding,
      possibly even for them to not build/test their result yet - too early.
      so in conclusion: with your updated HANDOFF+files commit, your next self should be completely able to create a workable bootstrap app.
