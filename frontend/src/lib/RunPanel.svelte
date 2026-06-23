<script lang="ts">
  import type { RunResponse } from './types';

  let {
    triggerInput = $bindable(),
    runResult,
    running
  }: {
    triggerInput: string;
    runResult: RunResponse | null;
    running: boolean;
  } = $props();

  function preview(output: unknown): string {
    if (output == null) return '—';
    if (typeof output === 'string') return output;
    return JSON.stringify(output, null, 2);
  }
</script>

<div class="panel">
  <div class="title">Run</div>
  <p class="hint">Select a block to configure it, or run the whole agent here.</p>

  <label class="field">
    <span class="flabel">Trigger input ({'{{input}}'})</span>
    <textarea rows="3" bind:value={triggerInput}></textarea>
  </label>

  {#if running}
    <div class="status">Running…</div>
  {/if}

  {#if runResult}
    <div class="result-head" class:err={runResult.status === 'error'}>
      Result: {runResult.status}
    </div>
    {#each runResult.results as r (r.node_id)}
      <div class="step" data-status={r.status}>
        <div class="step-head">
          <span class="dot {r.status}"></span>
          <code>{r.node_id}</code>
          <span class="ms">{r.ms}ms</span>
        </div>
        {#if r.error}
          <pre class="err-text">{r.error}</pre>
        {:else}
          <pre>{preview(r.output)}</pre>
        {/if}
      </div>
    {/each}
  {/if}
</div>

<style>
  .title {
    font-weight: 700;
    font-size: 16px;
  }
  .hint {
    font-size: 12px;
    color: #8a94ab;
    margin: 8px 0 14px;
    line-height: 1.4;
  }
  .field {
    display: block;
    margin-bottom: 14px;
  }
  .flabel {
    display: block;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: #6b7488;
    margin-bottom: 5px;
  }
  textarea {
    width: 100%;
    box-sizing: border-box;
    background: #141a28;
    border: 1px solid #232c40;
    border-radius: 6px;
    padding: 8px 10px;
    color: #e6e9ef;
    font-size: 13px;
    font-family: ui-monospace, monospace;
    resize: vertical;
  }
  .status {
    color: #c0a6ff;
    font-size: 13px;
  }
  .result-head {
    font-weight: 600;
    margin: 6px 0 10px;
    color: #10b981;
  }
  .result-head.err {
    color: #ef6a7d;
  }
  .step {
    background: #10141f;
    border: 1px solid #1c2333;
    border-radius: 8px;
    padding: 8px 10px;
    margin-bottom: 8px;
  }
  .step-head {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12px;
  }
  .ms {
    margin-left: auto;
    color: #6b7488;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #6b7488;
  }
  .dot.ok {
    background: #10b981;
  }
  .dot.error {
    background: #ef4444;
  }
  .dot.skipped {
    background: #6b7488;
  }
  pre {
    margin: 6px 0 0;
    font-size: 11px;
    white-space: pre-wrap;
    word-break: break-word;
    color: #c2c9d6;
    max-height: 200px;
    overflow: auto;
  }
  .err-text {
    color: #ef6a7d;
  }
</style>
