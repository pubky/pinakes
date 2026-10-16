
Web access interstitials are stored as `blocked` with no display fields or grouping evidence.
`queue.blocked_ttl_secs` controls probes (default 86400; 3600–2592000 seconds); these probes
never charge failures or dead-letter the resource. Access failure is not evidence of deletion:
accepted event dates and feed occurrences survive, and only two subsequent disappearance
observations can cancel them. `pinakes_blocked_resources{host}` reports the current count.
HTML meta-refresh and literal JavaScript redirects share the HTTP three-hop egress budget.
A meta refresh without a URL is an auto-reload and content; a refresh slower than ten seconds
is a hop only on a short page that does not refresh itself.

Wall heuristics live in `data/interstitial-markers.json` (text markers, captcha selectors,
wall host prefixes, sign-in paths, host overrides); `web.wall_markers_path` replaces the file
and `web.wall_overrides = { "example.org" = "content" }` (or `"blocked"`) decides a host and
its subdomains outright. Visible text excludes `script`, `style`, `noscript` and `template`.
Title, heading and captcha markers count only on short pages (under 2,000 visible bytes) or
challenge statuses (403/429/503); a redirect of the requested path to a listed sign-in path
(`/login`, `/authwall`, `/signin`, `/checkpoint`, …) is a wall. A new reference re-probes a
blocked page early, at most `ingest.blocked_reprobes_per_host_per_day` times per host per UTC
day (default 20; 0 disables) and not within an hour of the last attempt, so false positives
heal when someone cares about the page.

Redirect aliases do not establish group identity. Only explicit canonical/main-entity metadata,
validated sameAs evidence, or an entirely permanent HTTP redirect chain contributes URL keys.
A URL key claimed by three distinct paths within one publisher is demoted for every claimant;
the persistent demotion prevents unchanged metadata from reintroducing it. See
`pinakes_group_url_demotions_total`. Aliases remain available as navigation metadata.

Grouping conflicts use a separate durable retry counter with 5–10 seconds of initial jitter,
exponential growth and a one-hour ceiling. Saves respect that window, and the daemon drains
full batches continuously. Reference churn does not charge resource resolution attempts.
The pair budget scales as `max(100000, 2 * max_members²)`, capped at two million evaluations;
`max_members_per_origin` cannot exceed `max_members`. On exhaustion the partial plan preserves
established memberships, records `group_input.last_error='pair_budget'`, and increments
`pinakes_group_pair_budget_total` without repeatedly parking the same fingerprint.
Curation waits up to two seconds for the writer and returns 503 with Retry-After on contention.

Startup persists the grouping policy (including tenant rules) and re-dirties memberships when
it changes. For a manual rebuild use `DELETE FROM group_input; INSERT INTO group_dirty(resource_id)
SELECT DISTINCT resource_id FROM group_unit ON CONFLICT DO NOTHING;`. Existing retry windows
remain in force. Changing `groups.cross_container_occurrences` takes effect as that queue drains.

Tenant rules support `"subdomain"`, `"first_path_segment"` and `{"path_segment":3}` (one-based,
1–8). Prefix segments remain part of the scope. Tests cover the concrete URL shapes for every
seed, including constant-prefix Acast, Anchor, Google, SoundCloud and Podbean feed URLs.
Capability-only iCloud/Outlook path rules are removed; native calendars already use their
referenced feed identity as the container scope, without treating shared service paths as tenants.
The public feed shapes were checked against [Spotify's hosting guide](https://support.spotify.com/se/creators/article/switching-your-show-hosting-to-spotify-for-creators/),
[Acast's RSS guide](https://learn.acast.com/en/articles/3404332-how-to-add-your-podcast-to-tunein),
and [Podbean's feed guide](https://help.podbean.com/support/solutions/articles/25000004906-validating-my-podcast-feed).
Redirected RSS keeps the referenced URI as its publisher and cannot borrow the destination's
podcast GUID evidence. Migration 0027 isolates stale container origins and staggers re-fetches
for historical podcast cards without feed provenance.

The capability-URL guard scans every tracked source file and every nonignored new source file,
including root configuration, Rust and scripts. Git-ignored runtime credentials/artifacts and
`.git`, `target`, `node_modules` are excluded. Exact synthetic fixture URLs are allowed only
in named test files; failures print paths without URL values.
