# Greenhouse

Pinakes reads job postings on Greenhouse job boards from the public Job Board API, by board and job
ID. The board pages carry no structured data. No key is needed.

| | |
| --- | --- |
| Source | `greenhouse`, official source |
| Links | `job-boards.greenhouse.io/{board}/jobs/{id}`, its `boards.greenhouse.io` form, the application form and the API's address |
| Fetches | `GET https://boards-api.greenhouse.io/v1/boards/{board}/jobs/{id}?pay_transparency=true` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)), until a day after the posting's deadline |
| Card types | `JobPosting` |
| Keys | None (facts only) |
| Credit | None from Greenhouse; OpenStreetMap's when the workplace is placed on the map |
| Switch | `[sources.greenhouse] enabled`, on by default |

## Links

Every address of one posting is one resource. The root is
`https://job-boards.greenhouse.io/{board}/jobs/{id}`, and these links are
[aliases](../api.md#aliases) of it:

- `https://boards.greenhouse.io/{board}/jobs/{id}`;
- the application form, `/embed/job_app?for={board}&token={id}` on either host;
- the API's address, `https://boards-api.greenhouse.io/v1/boards/{board}/jobs/{id}`;
- any of these with other query parameters (`?gh_src=…`).

A board is at most 64 lower-case letters, digits, `-` and `_`. A job ID is a positive number of at
most 16 digits.

Not read here: a board's own page (`/{board}`) and a company's careers page that embeds a board
(`?gh_jid=`), which the web route reads.

## What it fetches

One request per read:
`GET https://boards-api.greenhouse.io/v1/boards/{board}/jobs/{id}?pay_transparency=true`. A posting
the board does not have (404: closed, or never posted) is `missing`. An answer for another posting,
one without a title, or any other status is an error and the read is retried.

When the posting names a workplace and geocoding is on, Nominatim is also asked where it is, within
3 seconds. A slow or unavailable geocoder leaves the card without a point.

## Card

The type is `JobPosting` ([Jobs](../api.md#jobs)).

- `name` is the posting's title. `description` is the start of its text, at most 300 characters of
  plain text.
- `start` is when the posting was first published.
- `end` is the application deadline, when it has one. A deadline given as a date alone lasts that
  whole day (UTC).
- `venue` is the workplace: the first place in the posting's location that is not remote. It is
  placed on the map as a career page's workplace is.

The location is a list separated by `;`. The first 32 places are read, each from its first 1,024
bytes. A place that says "Remote" ("Remote, United States", "Remote-Friendly, Europe") makes the
posting remote and names its regions. A posting can be remote and have a workplace.

| Fact | Meaning |
| --- | --- |
| `hiring_organization` | The company the board belongs to |
| `remote`, `remote_regions` | `true` when a place is remote, and the regions its remote places name |
| `salary_min`, `salary_max`, `salary_currency` | The first pay range the posting publishes, in the currency's main unit (not cents). A range without a minimum is left out |
| `date_posted`, `valid_through` | When the posting was first published and its application deadline, as Greenhouse gives them |
| `apply_url` | The posting's page on the company's board or its own careers page, when at most 256 bytes |
| `job_status` | `open`, or `closed` once the deadline has passed at the time of the read |
| `hiring_organization_url`, `employment_type`, `salary_unit` | Facts of every job posting that Greenhouse does not give: absent |

## Keys and groups

The source emits no keys. The links of one posting join as aliases of its root.

## Credit

The card credits nothing for Greenhouse's data. A workplace placed on the map carries OpenStreetMap's
attribution.

## Turn it off

```toml
[sources.greenhouse]
enabled = false
```

Greenhouse links then get ordinary web cards.

## Examples

- `https://job-boards.greenhouse.io/gitlab/jobs/8638232002`
- `https://boards.greenhouse.io/figma/jobs/5783812004`
