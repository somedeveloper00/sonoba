float4 main(float4 position : SV_POSITION) : SV_TARGET {
    float2 uv = position.xy / image_size;
    float dy = 1.0 / image_size.y;
    float3 sum = image.SampleLevel(nearest, uv + float2(0, -3 * dy), 0).rgb
               + image.SampleLevel(nearest, uv + float2(0, -2 * dy), 0).rgb
               + image.SampleLevel(nearest, uv + float2(0, -dy), 0).rgb
               + image.SampleLevel(nearest, uv, 0).rgb
               + image.SampleLevel(nearest, uv + float2(0, dy), 0).rgb
               + image.SampleLevel(nearest, uv + float2(0, 2 * dy), 0).rgb
               + image.SampleLevel(nearest, uv + float2(0, 3 * dy), 0).rgb;
    return float4(sum * (1.0 / 7.0), 1);
}
