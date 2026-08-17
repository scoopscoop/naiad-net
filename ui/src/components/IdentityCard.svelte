<script lang="ts">
  import type { FileDto } from '../lib/types';

  interface Props {
    file: FileDto;
  }
  let { file }: Props = $props();

  function fmtBytes(n: number): string {
    if (!Number.isFinite(n)) return '-';
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    let value = n;
    let unit = 0;
    while (value >= 1024 && unit < units.length - 1) {
      value /= 1024;
      unit += 1;
    }
    return `${unit === 0 ? value.toFixed(0) : value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`;
  }

  function fmtDate(seconds: number | null): string | null {
    if (seconds === null) return null;
    const d = new Date(seconds * 1000);
    if (Number.isNaN(d.getTime())) return null;
    const pad = (n: number) => String(n).padStart(2, '0');
    return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}`;
  }
</script>

<div class="card identity">
  <div class="row">
    <span class="key accent">BLAKE3</span>
    <span class="val">{file.hash}</span>
  </div>
  <div class="row">
    <span class="key">PATH</span>
    <span class="val dim">{file.path}</span>
  </div>
  <div class="row">
    <span class="key">SIZE</span>
    <span class="val">{fmtBytes(file.size)}</span>
  </div>
  <div class="row">
    <span class="key">FORMAT</span>
    <span class="val">{file.mime ?? '-'}</span>
  </div>
  {#if fmtDate(file.imported_at)}
    <div class="row">
      <span class="key">IMPORTED</span>
      <span class="val">{fmtDate(file.imported_at)}</span>
    </div>
  {/if}
  {#if fmtDate(file.created_at)}
    <div class="row">
      <span class="key">CREATED</span>
      <span class="val">{fmtDate(file.created_at)}</span>
    </div>
  {/if}
  {#if fmtDate(file.modified_at)}
    <div class="row">
      <span class="key">MODIFIED</span>
      <span class="val">{fmtDate(file.modified_at)}</span>
    </div>
  {/if}
</div>

<style>
  .card {
    background: var(--ink-900);
    border: 1px solid var(--line);
    border-radius: 9px;
    overflow: hidden;
  }
  .row {
    display: flex;
    align-items: baseline;
    gap: 10px;
    padding: 11px 13px;
  }
  .row + .row {
    border-top: 1px solid var(--raise);
  }
  .key {
    width: 58px;
    flex: none;
    font: 600 10px/1.4 var(--mono);
    color: var(--text-faint);
  }
  .key.accent {
    color: var(--accent);
  }
  .val {
    flex: 1;
    min-width: 0;
    font: 500 11.5px/1.4 var(--mono);
    color: var(--text);
    word-break: break-all;
  }
  .val.dim {
    color: var(--text-mute);
  }
</style>
