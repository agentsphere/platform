import { timeAgo } from '../lib/format';

interface Props {
  date: string;
  class?: string;
}

export function TimeAgo({ date, class: cls }: Props) {
  const absolute = new Date(date).toLocaleString();
  return (
    <span class={cls || 'text-muted text-sm'} title={absolute}>
      {timeAgo(date)}
    </span>
  );
}
