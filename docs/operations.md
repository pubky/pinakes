
Web access interstitials are stored as `blocked` with no display fields or grouping evidence.
`queue.blocked_ttl_secs` controls probes (default 86400; 3600–2592000 seconds); these probes
never charge failures or dead-letter the resource. Access failure is not evidence of deletion:
accepted event dates and feed occurrences survive, and only two subsequent disappearance
observations can cancel them. `pinakes_blocked_resources{host}` reports the current count.
HTML meta-refresh and literal JavaScript redirects share the HTTP three-hop egress budget.
