
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
