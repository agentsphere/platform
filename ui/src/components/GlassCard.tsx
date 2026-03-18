export function GlassCard({ children, className, hover }: { children: any; className?: string; hover?: boolean }) {
  return (
    <div class={`glass-card${hover ? ' glass-card-hover' : ''}${className ? ` ${className}` : ''}`}>
      {children}
    </div>
  );
}
