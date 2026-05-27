# Users and Workspaces

AeorDB ships with no implicit "home directory" for users. A new user
exists in the system store (`/.aeordb-system/users/<uuid>`) but owns
nothing in the file tree until an admin explicitly grants them access
to one or more paths.

This is deliberate. Different deployments want different patterns:

- An interactive customer-facing app might want every user to have a
  personal workspace at `/workspaces/<username>/`.
- A team-collaboration app might want shared folders at
  `/teams/<team-name>/` with no per-user space at all.
- A backup or sync deployment might want users that exist purely to
  hold API keys and never write a single file from the portal.

Hardcoding a `/home/<user>/` convention into the engine would force
every deployment into the first shape. Instead AeorDB exposes the
primitives — *create a folder*, *grant a permission* — and leaves the
policy to the operator.

## The suggested convention: `/workspaces/<username>/`

When an admin creates a user from the portal, the "Create User" modal
offers to bootstrap a personal workspace as a single extra step:

```
Username: [wyatt           ]
Email:    [wyatt@example.com]

☑ Grant a personal workspace
   Path: [/workspaces/wyatt  ]   (full access for this user)
```

Checking the box (default) performs two operations after the user
record is created:

1. `POST /files/mkdir` to create the folder at the chosen path.
2. `POST /files/share` to grant the new user `crudlify` permission on
   that folder.

The end result is identical to an admin doing both steps manually
from the file browser — it's a UX shortcut for the common case, not a
new engine concept. You can:

- **Override the path**: type whatever makes sense for your deployment
  (`/team-wyatt/`, `/u/wyatt/`, `/home/wyatt/`, …). The default is
  just a suggestion.
- **Skip the workspace entirely**: uncheck the box for service /
  backup / replication users who shouldn't have a portal-visible
  storage area.

## Users with no grants

A user with no grants and no shares is still a valid AeorDB account.
They can:

- Authenticate via `POST /auth/token` and receive a JWT.
- Hit any API endpoint the JWT permits (typically reads of paths
  shared with them later).
- Generate API keys for themselves (self-service if enabled).

What they can't do — yet — is see or write anything in the file tree.
The portal's file browser detects this state and shows a guidance
card with the user's ID so they can request access from an admin
instead of staring at a blank page with no action buttons.

## Granting access after the fact

If you skipped the workspace at user-creation time (or want to grant
additional folders later), the share flow is the canonical path:

1. From the file browser, navigate to (or create) the folder you want
   to grant.
2. Use the **Share** action on that folder.
3. Pick the user(s) and the permission level (`crudlify` for full
   access, or a custom subset).

This is the same engine call (`POST /files/share`) that the
user-creation modal makes when the workspace checkbox is on — it's
just split across the two operations (mkdir + share) when an admin
needs to do it incrementally.

## Service / backup / replication users

For automation accounts, the recommended pattern is:

1. Create the user with the workspace checkbox **unchecked**.
2. Generate an API key for that user from the Keys page.
3. Grant the user the *minimal* permission set their job needs — e.g.
   read-only access to `/data/exports/` for a backup job, or
   `crudlify` on `/incoming/<service-name>/` for an ingestion bot.
4. Hand the API key to the automation.

The user record will never appear in the portal's file browser
because there's nothing for them to browse — exactly the desired
behavior for non-interactive accounts.

## Why not auto-create `/home/<user>/`?

The first iteration of this design considered an engine-side rule
that would auto-create a home folder for every new user. We rejected
it because:

- It hardcodes a namespace. Customers wanting `/team-wyatt/` would
  have to navigate around an auto-created `/home/wyatt/` that they
  never asked for.
- Service users would get noise — every backup or CI account leaving
  an empty home folder for someone to clean up.
- Existing users (root, anyone created before the rule) would not
  have one — inconsistent and confusing.

Pushing the workspace creation to the *portal UI*, with an opt-out
checkbox and an editable path, gives the same 90%-case convenience
without baking a policy into the engine.
