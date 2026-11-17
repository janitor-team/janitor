{% extends "base.md" %}
{% block codemod -%}
{% if applied|length > 1 -%}
Fix some issues reported by lintian
{% endif %}

{% for entry in applied -%}
{% if applied|length > 1 %}* {% endif -%}{{ entry.summary }}{% if entry.fixed_lintian_tags %} ({% for tag in entry.fixed_lintian_tags %}[{{ tag }}](https://lintian.debian.org/tags/{{ tag }}){% if not loop.last %}, {% endif %}{% endfor %}){% endif %}

{% endfor %}
{% include "binary-diff.md" %}
{% endblock %}
