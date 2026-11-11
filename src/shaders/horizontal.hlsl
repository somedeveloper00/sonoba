float4 main(float4 position : SV_POSITION) : SV_TARGET {
    float2 uv = position.xy / image_size;
    float dx = 1.0 / image_size.x;
    float3 sum = image.SampleLevel(nearest, uv + float2(-3 * dx, 0), 0).rgb
               + image.SampleLevel(nearest, uv + float2(-2 * dx, 0), 0).rgb
               + image.SampleLevel(nearest, uv + float2(-dx, 0), 0).rgb
               + image.SampleLevel(nearest, uv, 0).rgb
               + image.SampleLevel(nearest, uv + float2(dx, 0), 0).rgb
               + image.SampleLevel(nearest, uv + float2(2 * dx, 0), 0).rgb
               + image.SampleLevel(nearest, uv + float2(3 * dx, 0), 0).rgb;
    return float4(sum * (1.0 / 7.0), 1);
}
