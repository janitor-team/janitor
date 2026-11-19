{%- set DEBDIFF_INLINE_THRESHOLD = 40 -%}
{% if role == 'main' and debdiff %}
{% if debdiff_is_empty(debdiff) %}

These changes have no impact on the [binary debdiff]({{ external_url }}/api/run/{{ log_id }}/debdiff?filter_boring=1).
{% elif debdiff|lines|length < DEBDIFF_INLINE_THRESHOLD %}

## Debdiff

These changes affect the binary packages:
{{ markdownify_debdiff(debdiff) }}
{% else %}

These changes affect the binary packages; see the [debdiff]({{ external_url }}/api/run/{{ log_id }}/debdiff?filter_boring=1)
{% endif %}

You can also view the [diffoscope diff]({{ external_url }}/api/run/{{ log_id }}/diffoscope?filter_boring=1) ([unfiltered]({{ external_url }}/api/run/{{ log_id }}/diffoscope)).
{% endif %}
