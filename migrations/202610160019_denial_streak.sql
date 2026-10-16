-- Optional adapter denials may preserve accepted facts briefly, never indefinitely.
ALTER TABLE resource ADD COLUMN denial_streak integer NOT NULL DEFAULT 0 CHECK(denial_streak>=0),
 ADD COLUMN denied_since timestamptz;
CREATE FUNCTION reset_denial_streak() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 IF NEW.last_error IS NULL OR NEW.last_error NOT IN('upstream_denied','graphql') THEN
  NEW.denial_streak=0; NEW.denied_since=NULL;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER reset_denial_streak BEFORE UPDATE OF last_error ON resource FOR EACH ROW EXECUTE FUNCTION reset_denial_streak();
