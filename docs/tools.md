# Tools and workflows

The tools are listed in the [README](../README.md#what-the-agent-can-do).
Their descriptions, which the agent reads, are the reference; this page
explains how they fit together.

## Finding notes

Every tool that takes a `note_ref` accepts either a note's internal ID or its
URL, such as `https://hackmd.io/@owner/slug`. If a URL matches no note, or more
than one, the tool does not guess: it returns the candidates so the agent can
pick.

Notes live in a workspace: your personal one, or a team's. Tools take a
`team_path` for a team and omit it for personal notes. `hackmd_get_me` lists the
teams you belong to.

`hackmd_list_notes` reads one of four lists through `source`:

| `source` | Lists |
|----------|-------|
| `workspace` (default) | notes in a personal or team workspace |
| `history` | the account's view history, in the order HackMD returns it |
| `trash` | trashed personal notes |
| `tracked` | local files synced by `hackmd_pull_note`, read without any request |

HackMD returns whole collections, so filtering, sorting, and paging happen in
the server. No list searches note bodies.

## Editing a note

The safe edit is a patch:

1. `hackmd_get_note` returns the body, a `patch_path`, and a `body_hash`.
2. The agent sends `hackmd_update_note` a patch in the `*** Begin Patch` format,
   headed with that exact `patch_path`, and passes `body_hash` back as
   `expected_hash`.

```text
*** Begin Patch
*** Update File: notes/<id>.md
@@ ## Action items
 - Ship the release notes
-- Fix the typo in the syllabus
+- Fix the typo in the syllabus (done)
*** End Patch
```

The patch is applied only if every hunk's context matches the current body
exactly once; ambiguous or missing context is an error, never a guess. Text
after `@@` is an anchor line that narrows where the hunk applies. A patch whose
`patch_path` names a different note is refused, so an edit prepared for one note
cannot land on another. If `expected_hash` no longer matches, someone changed
the note since the agent read it, and the write is refused. The hash is
optional, and HackMD has no conditional write: the check runs just before the
write, so it catches every change made before then but not one landing in the
instant between the two.

`content` replaces the whole body instead. It is for rewriting a note from
scratch and carries the destructive hint; nothing this server offers can undo
it.

A metadata-only update (title, tags, description, permalink, permissions,
folder) still sends the body: HackMD is believed to blank a body the PATCH
omits, so the server reads the current one and sends it back. That moves the
whole body twice more than a metadata change used to: one download before the
write and one upload with it. An edit landing between that read and the write
is reverted; `expected_hash` works here too and catches any made before it. A
title rarely shows through, because a YAML `title:` in the body wins, then a
leading H1, and only then the `title` field. A folder move is not waited on:
the result's `folder_placement_confirmed` says whether the read-back already
shows the note there (`null` when no folder was asked for).

No note PATCH is retried, even after a rate limit. Most carry a body read or
checked just before (a patch, a push, a metadata update, `content` with
`expected_hash`), and even a plain `content` replacement, which reads nothing
first, would land after the backoff over edits made in the meantime. The
error goes back to the agent, which reads again.

Body edits are read back until HackMD shows them, because some writes become
visible only after a delay. Folder updates and a new note's folder placement
are read back the same way; deletions, restores, folder creation, and image
uploads are reported as HackMD acknowledged them. A write that may or may not
have landed (the connection dropped, HackMD answered 5xx, or it answered
success with a reply that could not be read) is reported as unconfirmed with
kind `readback`, and the agent is told to look rather than retry. Image uploads
are the exception: nothing can look for an uploaded image, and a second upload
only leaves an unused copy, so an unreadable reply there is `upstream`.

## Pull, edit locally, push

Sync turns a note into a Markdown file that any editor, script, or git
repository can work with:

1. `hackmd_pull_note` writes the note to an absolute `.md` path and records a
   baseline: the exact body as it was at pull time. It returns that body's
   `body_hash`, in the same form `hackmd_get_note` and a successful push report.
2. You or the agent edit the file locally.
3. `hackmd_get_note` with `local_path` reports where things stand by comparing
   the file, the baseline, and the remote note:

   | State | Meaning |
   |-------|---------|
   | `in_sync` | nothing to do |
   | `local_changed` | only the file changed; push it |
   | `remote_changed` | only the note changed; pull with `overwrite_local: true` |
   | `conflict` | both changed |

4. `hackmd_push_note` writes the file back. In the default `safe` strategy it
   re-reads the note right before writing, and if the note changed since the
   pull it writes nothing.

On a conflict, push returns a short diff, saves the current remote body next to
your file as `<name>.remote.md`, and reports its `remote_body_hash`. Merge the
two, then push again with `expected_remote_hash` set to that hash: the push
goes through only if the remote still has exactly that body. A `.remote.md` you
edited yourself is never overwritten by a later conflict.

`strategy: overwrite` with `confirm: true` replaces the remote regardless. A
pull over any existing file needs `overwrite_local: true`. Even then, a pull
over a tracked file with unpushed edits is refused unless
`discard_local_changes: true` is given, and so is a pull over a file with no
usable sync record (none, or a broken one) whose content differs from the
note, such as one fetched by other means and edited since: nothing shows
those edits were ever pushed. To keep them, pull to another path, carry the
edits into that file, and push it. A file that already matches the note is
adopted, and tracked from then on. `hackmd_untrack_note`, given the `note_id`
and `confirm: true`, forgets the sync record without touching the file or the
note; a later pull over that file, once edited, needs `discard_local_changes`
like any other unrecorded file.

## Folders, deletion, images

- `hackmd_update_folder` updates team folder metadata and sets folder order
  with `child_order`, which may name only the folder's own children. The
  read-back confirms HackMD stored the order; the team order route is inferred
  from the personal one and unmeasured. HackMD reports folder moves as
  successful while doing nothing, so moves are refused outright. Personal
  folder metadata updates are refused too, since nothing confirms they take
  effect.
- `hackmd_delete_folder` leaves a folder that has child folders alone unless
  `confirm: true` is given. HackMD does not report which notes a folder holds,
  so check `folder_ids` from `hackmd_get_note` first if that matters.
- `hackmd_delete_note` with `restore: true` brings a personal note back from
  trash. Team notes cannot be restored through the API.
- `hackmd_upload_note_image` uploads an image, from a local file or a public
  URL, to a personal or team note and returns its CDN link. The link is public whenever the note is
  guest-readable; an anonymous fetch of an image on an owner-only note was
  refused when measured, but treat that as observed, not promised. So the
  result also reports `publicly_readable`: right after the upload the server
  sends one signed-out `HEAD` for the link, without the token and only to
  HackMD's own site, and reports `true` if an image or a redirect to a
  presigned storage URL comes back, `false` if the request is refused, and
  `null` if the check could not run or proved nothing (any other redirect,
  such as to a login page, counts as nothing). It is always `null` when the
  API is neither on the site's own host nor on its `api.` subdomain. A
  `false` usually means the note is not guest-readable, and signed-out
  readers will not see the image until it is. A local `image_path` needs a
  workspace root (see [configuration.md](configuration.md#workspace-root)).
  Instead of a local file, `image_url` re-hosts an image from a public URL,
  which is how a note's imgur links move to HackMD without a download step.
  The server fetches it only over `https` on the default port, only from a
  host whose every resolved address is public (loopback, private, link-local,
  CGNAT and reserved ranges are refused, and the checked addresses are pinned
  for the connection), and re-checks each of at most 5 redirects the same way.
  An address check cannot see translation beyond the server: on an IPv6-only
  network whose NAT64 gateway uses its own prefix, a private IPv4 address can
  arrive looking public, so such a network must filter that at its egress.
  Nor can it see whom a public host serves: one that answers only your
  network, by source address, passes, and its image is republished like any
  other. A `rate_limited` or `upstream` error from an `image_url` fetch is
  about the image host, not HackMD, and so is a `network` error naming a host
  that does not resolve (the server cannot tell a missing host from DNS being
  down). The
  size an image host declares is checked before the download, and without
  `confirm_large_file` an undeclared one stops at 5 MiB. A URL upload is named
  after the URL's last path segment, with the extension of the type its bytes
  show. Either source must be a PNG, JPEG, GIF, or WebP by its leading bytes;
  files over 5 MiB need `confirm_large_file: true` and files over 10 MiB are
  refused.

## Errors

A failed tool call carries a human-readable message that names the fix and a
stable `_meta.error_kind` that a client can branch on. Retries the server made
on its own are reported in `_meta.retry`.
