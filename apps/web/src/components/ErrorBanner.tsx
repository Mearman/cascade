interface Props {
  message: string;
  onDismiss?: () => void;
}

export function ErrorBanner({ message, onDismiss }: Props) {
  return (
    <div class="error-banner" role="alert">
      <span>{message}</span>
      {onDismiss && (
        <button class="dismiss" onClick={onDismiss} aria-label="Dismiss">
          x
        </button>
      )}
    </div>
  );
}
