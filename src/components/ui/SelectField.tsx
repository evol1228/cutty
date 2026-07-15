// Dark-themed select shared across forms: WebKitGTK's native `<select>`
// chrome ignores our colors (white pill, invisible text), so reset
// appearance and draw our own chevron.

function SelectField({
  id,
  value,
  onChange,
  options,
}: {
  id: string;
  value: string;
  onChange: (value: string) => void;
  options: { value: string; label: string }[];
}) {
  return (
    <div className="relative">
      <select
        id={id}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="w-full appearance-none rounded-md border border-zinc-700 bg-zinc-800 py-1.5 pl-2 pr-8 text-sm text-zinc-100"
      >
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
      <span className="pointer-events-none absolute inset-y-0 right-2.5 flex items-center text-xs text-zinc-500">
        ▾
      </span>
    </div>
  );
}

export default SelectField;
