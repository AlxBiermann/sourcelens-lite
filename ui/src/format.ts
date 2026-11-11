const numberFormat = new Intl.NumberFormat('en-US');
const dateFormat = new Intl.DateTimeFormat('en-GB', { day: 'numeric', month: 'short', year: 'numeric' });

export function count(n: number): string {
  return numberFormat.format(n);
}

export function plural(n: number, singular: string, pluralForm = `${singular}s`): string {
  return `${count(n)} ${n === 1 ? singular : pluralForm}`;
}

export function date(ts: number): string {
  return dateFormat.format(new Date(ts * 1000));
}

export function ago(ts: number, now = Date.now() / 1000): string {
  const seconds = Math.max(0, Math.round(now - ts));
  if (seconds < 45) return 'just now';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.round(hours / 24);
  if (days < 30) return `${days} ${days === 1 ? 'day' : 'days'} ago`;
  return date(ts);
}

export function duration(start: number | null, end: number | null): string {
  if (start === null) return '';
  const total = Math.max(0, Math.round((end ?? Date.now() / 1000) - start));
  if (total < 60) return `${total} s`;
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  if (minutes < 60) return seconds ? `${minutes} min ${seconds} s` : `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  return `${hours} h ${minutes % 60} min`;
}

export function shortSha(sha: string | null): string {
  return sha ? sha.slice(0, 7) : '';
}
