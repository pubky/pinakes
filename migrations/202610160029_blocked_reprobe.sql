-- Demand-driven re-probes of blocked pages: a new reference may pull the next attempt
-- forward, bounded per host per day so tag floods cannot turn into fetch floods.
CREATE TABLE blocked_reprobe(
 host text NOT NULL,
 day date NOT NULL,
 count integer NOT NULL CHECK(count>0),
 PRIMARY KEY(host,day)
);
