# Greenhouse

Job postings on Greenhouse job boards, read from the public Job Board API. The board pages
carry no structured data. No key; no published limit for reads.

## Links

`https://job-boards.greenhouse.io/{board}/jobs/{id}` is the root; the `boards.greenhouse.io`
spelling, the application form (`/embed/job_app?for={board}&token={id}`) and the API's address
are aliases of it. A company's own careers page that embeds a board (`?gh_jid=`) is read as a
web page.

## What it reads

`GET https://boards-api.greenhouse.io/v1/boards/{board}/jobs/{id}?pay_transparency=true`. A
posting the board no longer has (closed, or never posted) is `missing`.

## Card

A `JobPosting` ([Jobs](../api.md#jobs)) named by its title and described by the start of its
text. Its `start` is when it was first published; its `end` is its application deadline, when it
has one. The first place in its location that is not remote is its workplace, placed on the map
as a career page's is; a remote place ("Remote, United States", "Remote-Friendly, Europe") makes
it remote and names its regions.

| Fact | Meaning |
| --- | --- |
| `hiring_organization` | The company the board belongs to |
| `remote`, `remote_regions` | `true`, and the regions its remote places name |
| `salary_min`, `salary_max`, `salary_currency` | The first pay range the posting publishes; Greenhouse gives no unit |
| `date_posted`, `valid_through` | When it was first published and its application deadline, as Greenhouse gives them |
| `apply_url` | The posting's page on the company's board (or its own careers page) |
| `job_status` | `open`, or `closed` once its deadline has passed |

## Examples

- `https://job-boards.greenhouse.io/gitlab/jobs/8638232002`
- `https://boards.greenhouse.io/figma/jobs/5783812004`
