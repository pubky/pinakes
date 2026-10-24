# `wikipedia`: the other editions of a Wikipedia article

An [also on](../api.md#also-on) source. An article's card comes from its Wikipedia's REST summary ([the API](../api.md#also-on)).

`wikipedia`: the article's own Wikipedia names its Wikidata item (following
redirects); the item's other language editions are listed as `Wikipedia ({lang})`, the major
languages first, with its Britannica, Stanford Encyclopedia of Philosophy, Internet Encyclopedia
of Philosophy, Scholarpedia and Namuwiki entries, and its Q-id is the key, so two editions of one
article join. Titles are never matched across languages.

## Operations

Each Wikipedia article costs one request to its Wikipedia and one to Wikidata.
