interface ReplyBannerProps {
  agentLabel: string;
  agentColor: string;
  onDismiss: () => void;
}

export function ReplyBanner({ agentLabel, agentColor, onDismiss }: ReplyBannerProps) {
  return (
    <div class="reply-banner" style={`border-left-color: ${agentColor}`}>
      <span class="reply-banner-text">
        Replying to <strong>{agentLabel}</strong>
      </span>
      <button class="reply-banner-dismiss" onClick={onDismiss} type="button" aria-label="Cancel reply">
        &times;
      </button>
    </div>
  );
}
