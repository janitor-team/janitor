{# Merge proposal description for the trailing-whitespace campaign of
   examples/strip-trailing-whitespace, on forges that take Markdown. "files"
   is from the context that the codemod writes to $SVP_RESULT. #}
Strip trailing whitespace from these files:

{% for file in files %}
* `{{ file }}`
{% endfor %}
{% if external_url %}

Build and test logs: {{ external_url }}/cupboard/run/{{ log_id }}/
{% endif %}
