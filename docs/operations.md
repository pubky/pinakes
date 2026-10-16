
Web access interstitials are stored as `blocked` with no display fields or grouping evidence.
`queue.blocked_ttl_secs` controls probes (default 86400; 3600–2592000 seconds); these probes
never charge failures or dead-letter the resource. Access failure is not evidence of deletion:
accepted event dates and feed occurrences survive, and only two subsequent disappearance
observations can cancel them. `pinakes_blocked_resources{host}` reports the current count.
HTML meta-refresh and literal JavaScript redirects share the HTTP three-hop egress budget.

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
