<script lang="ts">
  // Global styles (Tailwind preflight + dark theme reset) load here so the
  // body background, margins and text colour apply app-wide.
  import '../app.css';

  let { children } = $props();
</script>

{@render children()}
