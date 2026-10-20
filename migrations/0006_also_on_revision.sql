-- A source's keys and facts are searched (`key=`): their changes count as search writes, as
-- derived links' do (`derived_link_revision`), so pruning an expired fact changes search ETags.
CREATE TRIGGER resource_key_revision BEFORE INSERT OR DELETE OR UPDATE ON resource_key FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER resource_fact_revision BEFORE INSERT OR DELETE OR UPDATE ON resource_fact FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
