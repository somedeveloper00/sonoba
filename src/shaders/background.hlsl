float4 main(float4 position : SV_POSITION) : SV_TARGET {
    if (any(position.xy >= image_size)) discard;
    float2 inside = position.xy - box.xy;
    float2 edge = min(inside, box.zw - inside);
    float alpha = saturate(min(edge.x, edge.y) * (1.0 / 3.0));
    float3 color = image.SampleLevel(nearest, position.xy / image_size, 0).rgb;
    return float4(color * alpha, alpha);
}
