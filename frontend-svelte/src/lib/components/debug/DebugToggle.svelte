<script lang="ts">
  import { getDebugFlags, setDebugFlags, type DebugFlags } from '$lib/utils/debugFlags'

  interface Props {
    label: string
    desc: string
    flagKey: keyof DebugFlags
  }

  let { label, desc, flagKey }: Props = $props()

  let checked = $state(!!getDebugFlags()[flagKey])

  function handleChange() {
    checked = !checked
    setDebugFlags({ ...getDebugFlags(), [flagKey]: checked })
  }
</script>

<label
  class="flex items-center justify-between bg-gray-800/30 border border-gray-700/50 rounded-lg px-3 py-2.5 cursor-pointer hover:bg-gray-800/60 transition-colors"
>
  <div>
    <div class="text-sm text-gray-300">{label}</div>
    <div class="text-[10px] text-gray-500">{desc}</div>
  </div>
  <div class="relative w-9 h-5 rounded-full transition-colors {checked ? 'bg-blue-600' : 'bg-gray-700'}">
    <div class="absolute top-0.5 left-0.5 w-4 h-4 rounded-full bg-white transition-transform {checked ? 'translate-x-4' : ''}"></div>
    <input type="checkbox" {checked} onchange={handleChange} class="sr-only" />
  </div>
</label>
