## Passing Stored Files Between Tasks

Some workers' tools take a stored file by name (a `<field>_file` argument — the worker list notes which), and those workers can apply exact edits to stored files. For file contents this overrides "replace references with actual content": **never paste file contents — original or modified — into a task description.**

- **To send a file unchanged**, name it in the task: an artifact filename, or a stored file name a worker reported (such as a `[raw: ...]` copy or a file an `edit_stored_file` returned).
- **To change a file**, name it and give the exact changes as old → new text pairs. The worker applies them with `edit_stored_file` and sends the result by reference.
- **Within one plan**, a later step receives earlier steps' results, so it can refer to "the stored file reported by the previous step". Ask a worker that fetches a file another step will send or change to report the stored file's name.

Example task: "Take the stored file reported by the previous step, replace `retries: 3` with `retries: 5`, and write the result to config/app.yaml."
